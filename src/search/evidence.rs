use super::*;

type Card = (String, String, usize, usize);

#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    prepared: &Prepared,
    query: &str,
    probabilities: &BTreeMap<usize, Option<f64>>,
    best: &BTreeMap<usize, f64>,
    fresh: &BTreeSet<usize>,
    followup_targets: &BTreeSet<usize>,
    threshold: f64,
    mut reserved: Reservation,
    policy: Policy,
    control: &mut dyn FnMut() -> Control,
) -> Option<(Card, VecDeque<Job>)> {
    let mut accepted: Vec<(usize, f64)> = probabilities
        .iter()
        .filter_map(|(&index, &probability)| {
            probability.filter(|&p| p >= threshold).map(|p| (index, p))
        })
        .collect();
    accepted.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    if accepted.is_empty() {
        return None;
    }
    let records = merge(prepared, &accepted, fresh);
    let mut text = String::new();
    for record in &records {
        let excerpt = format!(
            "// {}:{}-{}\n{}",
            record.path, record.start_line, record.end_line, record.excerpt
        );
        let separator = usize::from(!text.is_empty());
        if text.len() + separator + excerpt.len() > EVIDENCE_BYTES {
            break;
        }
        if separator == 1 {
            text.push('\n');
        }
        text.push_str(&excerpt);
    }
    if text.is_empty() {
        let record = records.first()?;
        let excerpt = format!(
            "// {}:{}-{}\n{}",
            record.path, record.start_line, record.end_line, record.excerpt
        );
        let mut end = EVIDENCE_BYTES.min(excerpt.len());
        while !excerpt.is_char_boundary(end) {
            end -= 1;
        }
        text = excerpt[..end].to_owned();
    }
    let top = records.first().expect("nonempty evidence");
    let card = (text.clone(), top.path.clone(), top.start_line, top.end_line);
    let evidence = Candidate {
        name: "evidence",
        path: &top.path,
        text: &text,
        start_line: top.start_line,
        end_line: top.end_line,
    };
    let accepted_files: BTreeSet<usize> = accepted
        .iter()
        .map(|&(index, _)| prepared.windows[index].file)
        .collect();
    let mut targets: Vec<(u8, usize, usize, f64)> = Vec::new();
    for (&index, &score) in best {
        if control() != Control::Continue {
            return None;
        }
        if score >= threshold
            || probabilities
                .get(&index)
                .copied()
                .flatten()
                .is_some_and(|p| p >= threshold)
        {
            continue;
        }
        let window = &prepared.windows[index];
        let (category, distance) = if accepted_files.contains(&window.file) {
            let distance = accepted
                .iter()
                .filter(|&&(accepted_index, _)| {
                    prepared.windows[accepted_index].file == window.file
                })
                .map(|&(accepted_index, _)| {
                    window
                        .start_line
                        .saturating_sub(prepared.windows[accepted_index].end_line)
                        .max(
                            prepared.windows[accepted_index]
                                .start_line
                                .saturating_sub(window.end_line),
                        )
                })
                .min()
                .unwrap_or(0);
            (0u8, distance)
        } else if followup_targets.contains(&index) {
            (1, 0)
        } else {
            (2, 0)
        };
        targets.push((category, distance, index, score));
    }
    targets.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(b.3.total_cmp(&a.3))
            .then(a.2.cmp(&b.2))
    });
    let ready = tokio::time::Instant::now();
    let mut jobs = VecDeque::new();
    for chunk in targets.chunks(EVIDENCE_BATCH).take(EVIDENCE_JOBS) {
        if control() != Control::Continue {
            break;
        }
        let names: Vec<_> = chunk
            .iter()
            .map(|&(_, _, index, _)| format!("w{index}"))
            .collect();
        let candidates: Vec<_> = chunk
            .iter()
            .zip(&names)
            .map(|(&(_, _, index, _), name)| candidate(prepared, index, name))
            .collect();
        let Some(batch) = Batch::encode_with_context(query, &candidates, &evidence, EVIDENCE_LIMIT)
            .ok()
            .flatten()
        else {
            continue;
        };
        let Some(next) = reserve(
            reserved,
            batch.encoded_len(),
            policy.max_attempts,
            policy.max_bytes,
        ) else {
            break;
        };
        reserved = next;
        jobs.push_back(Job {
            batch,
            purpose: Purpose::Evidence {
                targets: chunk.iter().map(|&(_, _, index, _)| index).collect(),
            },
            pending_retry: None,
            refusal_retry: false,
            ready,
        });
    }
    Some((card, jobs))
}
