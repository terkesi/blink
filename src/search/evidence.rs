use super::*;

pub(super) type Card = (String, String, usize, usize, BTreeSet<usize>);

/// Plans the evidence re-asks. The card, its targets and their chunks do not depend on
/// `relation`: with it, each batch's `evidence` slot starts with every target's relation context
/// and continues with the unchanged card, within the same per-batch allowance, and keeps the
/// card alone when no target has any context or none of it fits. The slot's actual bytes are
/// what the pass reserves, so when the byte ceiling binds the planned tail of this pass, up to
/// the whole pass, can be shorter than without it; every chunk dropped that way records a stop.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    prepared: &Prepared,
    query: &str,
    probabilities: &BTreeMap<usize, Option<f64>>,
    best: &BTreeMap<usize, f64>,
    fresh: &BTreeSet<usize>,
    followup_targets: &BTreeSet<usize>,
    nominated: &BTreeMap<usize, f64>,
    threshold: f64,
    mut reserved: Reservation,
    policy: Policy,
    relation: bool,
    control: &mut dyn FnMut() -> Control,
) -> Option<(Card, VecDeque<Job>, Option<&'static str>)> {
    let accepted: Vec<(usize, f64)> = probabilities
        .iter()
        .filter_map(|(&index, &probability)| {
            probability
                .filter(|&p| p >= threshold && fresh.contains(&prepared.windows[index].file))
                .map(|p| (index, p))
        })
        .collect();
    if accepted.is_empty() {
        return None;
    }
    let mut records = merge(prepared, &accepted, fresh);
    records.sort_by(|a, b| {
        b.probability
            .total_cmp(&a.probability)
            .then(a.path.cmp(&b.path))
            .then(a.start_byte.cmp(&b.start_byte))
    });
    let excerpts: Vec<String> = records
        .iter()
        .map(|record| {
            format!(
                "// {}:{}-{}\n{}",
                record.path, record.start_line, record.end_line, record.excerpt
            )
        })
        .collect();
    let mut kept = 0;
    let mut length = 0;
    for excerpt in &excerpts {
        if length + usize::from(kept > 0) + excerpt.len() > EVIDENCE_BYTES {
            break;
        }
        length += usize::from(kept > 0) + excerpt.len();
        kept += 1;
    }
    let card_text = |kept: usize| -> String {
        if kept == 0 {
            let excerpt = &excerpts[0];
            let mut end = EVIDENCE_BYTES.min(excerpt.len());
            while !excerpt.is_char_boundary(end) {
                end -= 1;
            }
            return excerpt[..end].to_owned();
        }
        excerpts[..kept].join("\n")
    };
    let top = &records[0];
    let probe_names: Vec<String> = (0..EVIDENCE_BATCH).map(|n| format!("p{n}")).collect();
    let probes: Vec<_> = probe_names
        .iter()
        .map(|name| candidate(prepared, accepted[0].0, name))
        .collect();
    let mut text = card_text(kept);
    loop {
        let evidence = Candidate {
            name: "evidence",
            path: &top.path,
            text: &text,
            start_line: top.start_line,
            end_line: top.end_line,
        };
        let fits = matches!(
            Batch::encode_with_context(query, &probes, &evidence, EVIDENCE_LIMIT),
            Ok(Some(_))
        );
        if fits || text.len() < 64 {
            break;
        }
        if kept > 1 {
            kept -= 1;
            text = card_text(kept);
        } else {
            kept = 0;
            let mut end = text.len() / 2;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
    }
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
    let kept_paths: BTreeSet<&str> = records[..kept.max(1)]
        .iter()
        .map(|record| record.path.as_str())
        .collect();
    let card_files: BTreeSet<usize> = accepted_files
        .iter()
        .copied()
        .filter(|&file| kept_paths.contains(prepared.snapshot.files()[file].path()))
        .collect();
    let card = (
        text.clone(),
        top.path.clone(),
        top.start_line,
        top.end_line,
        card_files,
    );
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
        let (category, distance) = if let Some(choice) = nominated.get(&index) {
            (0u8, ((1.0 - choice) * 1000.0) as usize)
        } else if accepted_files.contains(&window.file) {
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
            (1u8, distance)
        } else if followup_targets.contains(&index) {
            (2, 0)
        } else {
            (3, 0)
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
    let mut relations = (relation && !targets.is_empty())
        .then(|| relation::Index::new(prepared, query, control))
        .flatten();
    let judged = relation::Judged {
        probabilities,
        fresh,
        threshold,
    };
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
        // The relation context precedes the unchanged card in the same slot, so a chunk sent
        // with context would also have been sent with the card alone: the planned chunks and
        // their partners do not depend on the context.
        let related = relations.as_mut().and_then(|index| {
            let contexts: Vec<_> = chunk
                .iter()
                .map(|&(_, _, target, _)| index.relation(target, &judged, None))
                .collect();
            relation::encode(
                query,
                &candidates,
                &contexts,
                evidence,
                Some(evidence.text),
                EVIDENCE_LIMIT,
            )
        });
        let Some(batch) = related.or_else(|| {
            Batch::encode_with_context(query, &candidates, &evidence, EVIDENCE_LIMIT)
                .ok()
                .flatten()
        }) else {
            continue;
        };
        let Some(next) = reserve(
            reserved,
            batch.encoded_len(),
            policy.max_attempts,
            policy.max_bytes,
        ) else {
            return Some((card, jobs, Some(limit_stop(reserved, policy.max_attempts))));
        };
        reserved = next;
        jobs.push_back(Job {
            batch,
            purpose: Purpose::Evidence {
                targets: chunk.iter().map(|&(_, _, index, _)| index).collect(),
            },
            pending_retry: None,
            retries_done: 0,
            refusal_retry: false,
            ready,
        });
    }
    Some((card, jobs, None))
}
