use super::*;

/// Plans plain source requests over the unread windows of files that already hold an accepted
/// window, nearest to the accepted source first. Files are ordered by their strongest acceptance.
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
        if let Some(p) = probability.filter(|&p| p >= threshold) {
            let file = prepared.windows[index].file;
            if fresh.contains(&file) {
                let entry = strength.entry(file).or_insert(p);
                *entry = entry.max(p);
            }
        }
    }
    let mut candidates: Vec<(usize, usize, usize)> = Vec::new();
    for (&file, &p) in &strength {
        let accepted: Vec<&Window> = probabilities
            .iter()
            .filter(|(index, probability)| {
                prepared.windows[**index].file == file
                    && probability.is_some_and(|p| p >= threshold)
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
        let rank = ((1.0 - p) * 1000.0) as usize;
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
