use super::*;

/// Plans plain source requests over the unread windows of implicated files: files that hold an
/// accepted window first, then files whose strongest judged window reached `DEEPEN_FLOOR`. Within a
/// file, windows nearest to its strongest judged source come first.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    prepared: &Prepared,
    query: &str,
    probabilities: &BTreeMap<usize, Option<f64>>,
    fresh: &BTreeSet<usize>,
    threshold: f64,
    mut reserved: Reservation,
    policy: Policy,
    control: &mut dyn FnMut() -> Control,
) -> VecDeque<Job> {
    let mut strength: BTreeMap<usize, f64> = BTreeMap::new();
    for (&index, probability) in probabilities {
        if let Some(p) = probability.filter(|&p| p >= DEEPEN_FLOOR) {
            let file = prepared.windows[index].file;
            if p < threshold || fresh.contains(&file) {
                let entry = strength.entry(file).or_insert(p);
                *entry = entry.max(p);
            }
        }
    }
    let mut candidates: Vec<(usize, usize, usize)> = Vec::new();
    for (&file, &p) in &strength {
        let anchor = if p >= threshold { threshold } else { p };
        let accepted: Vec<&Window> = probabilities
            .iter()
            .filter(|(index, probability)| {
                prepared.windows[**index].file == file && probability.is_some_and(|q| q >= anchor)
            })
            .map(|(&index, _)| &prepared.windows[index])
            .collect();
        let unread: Vec<usize> = (0..prepared.windows.len())
            .filter(|index| {
                prepared.windows[*index].file == file && !probabilities.contains_key(index)
            })
            .collect();
        if unread.len() < DEEPEN_MIN_UNREAD {
            continue;
        }
        let rank = usize::from(p < threshold) * 1000 + ((1.0 - p) * 1000.0) as usize;
        for index in unread {
            let window = &prepared.windows[index];
            let distance = accepted
                .iter()
                .map(|a| {
                    window
                        .start_line
                        .saturating_sub(a.end_line)
                        .max(a.start_line.saturating_sub(window.end_line))
                })
                .min()
                .unwrap_or(0);
            candidates.push((rank, distance, index));
        }
    }
    candidates.sort();
    let ready = tokio::time::Instant::now();
    let mut jobs = VecDeque::new();
    for chunk in candidates.chunks(DEEPEN_BATCH).take(DEEPEN_JOBS) {
        if control() != Control::Continue {
            break;
        }
        let indices: Vec<usize> = chunk.iter().map(|&(_, _, index)| index).collect();
        let Ok(batch) = source_batch(prepared, query, &indices) else {
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
            purpose: Purpose::Source(indices),
            pending_retry: None,
            refusal_retry: false,
            ready,
        });
    }
    jobs
}
