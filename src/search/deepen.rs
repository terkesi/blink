use super::*;

/// Plans plain source requests over the unread windows of implicated files, in four tiers: files
/// that hold an accepted window, then files whose strongest judged window reached `DEEPEN_FLOOR`,
/// then files that declare something accepted code calls, then (only once something was accepted)
/// the files whose paths and text match the query's words most. Within a file, windows nearest to
/// the anchor (accepted, strongest, declaring, or best-matching window) come first. Files already
/// found to have changed are skipped in every tier.
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
    let stale: BTreeSet<usize> = probabilities
        .iter()
        .filter_map(|(&index, probability)| {
            probability
                .filter(|&p| p >= threshold)
                .map(|_| prepared.windows[index].file)
        })
        .filter(|file| !fresh.contains(file))
        .collect();
    let mut strength: BTreeMap<usize, f64> = BTreeMap::new();
    for (&index, probability) in probabilities {
        if let Some(p) = probability.filter(|&p| p >= threshold.min(DEEPEN_FLOOR)) {
            let file = prepared.windows[index].file;
            if !stale.contains(&file) {
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
    // Third tier: files that declare a function, type or constant which accepted code calls, read
    // from the declaring window outwards. The call graph implicates them even when neither the
    // model's scores nor the query's words do.
    let accepted_windows: Vec<usize> = probabilities
        .iter()
        .filter(|(index, probability)| {
            probability.is_some_and(|p| p >= threshold)
                && fresh.contains(&prepared.windows[**index].file)
        })
        .map(|(&index, _)| index)
        .collect();
    let text = |index: usize| {
        let window = &prepared.windows[index];
        &prepared.snapshot.files()[window.file].text()[window.start..window.end]
    };
    let mut called: BTreeSet<&str> = BTreeSet::new();
    for &index in &accepted_windows {
        called.extend(callees::calls(text(index)));
    }
    // Names the accepted code declares itself are local calls, not leads to other files.
    for &index in &accepted_windows {
        for name in callees::declarations(text(index)) {
            called.remove(name);
        }
    }
    // file -> (how many files declare the rarest matched name, anchor window)
    let mut callee_files: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    if !called.is_empty() {
        let mut declaring: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
        for index in 0..prepared.windows.len() {
            if control() != Control::Continue {
                return VecDeque::new();
            }
            let file = prepared.windows[index].file;
            if strength.contains_key(&file) || stale.contains(&file) {
                continue;
            }
            for name in callees::declarations(text(index)) {
                if called.contains(name) {
                    declaring.entry(name).or_default().push((file, index));
                }
            }
        }
        for windows in declaring.values() {
            let files: BTreeSet<usize> = windows.iter().map(|&(file, _)| file).collect();
            let rarity = files.len();
            for &(file, index) in windows {
                let entry = callee_files.entry(file).or_insert((rarity, index));
                if rarity < entry.0 || (rarity == entry.0 && index < entry.1) {
                    *entry = (rarity, index);
                }
            }
        }
    }
    let mut callee_order: Vec<(usize, usize, usize)> = callee_files
        .iter()
        .map(|(&file, &(rarity, anchor))| (rarity, file, anchor))
        .collect();
    callee_order.sort();
    callee_order.truncate(DEEPEN_LEXICAL_FILES);
    for (position, &(_, file, anchor_index)) in callee_order.iter().enumerate() {
        let unread: Vec<usize> = (0..prepared.windows.len())
            .filter(|index| {
                prepared.windows[*index].file == file && !probabilities.contains_key(index)
            })
            .collect();
        if unread.len() < DEEPEN_MIN_UNREAD {
            continue;
        }
        let anchor = &prepared.windows[anchor_index];
        for index in unread {
            let window = &prepared.windows[index];
            // Overlapping neighbours sit at distance zero too, so the declaring window itself
            // comes first and its neighbours follow by line distance.
            let distance = if index == anchor_index {
                0
            } else {
                window
                    .start_line
                    .saturating_sub(anchor.end_line)
                    .max(anchor.start_line.saturating_sub(window.end_line))
                    + 1
            };
            candidates.push((2000 + position, distance, index));
        }
    }
    // Fourth tier, only once something was accepted: the files the query's own words point at most
    // strongly, even when every window read there so far scored low.
    let mut lexical: BTreeMap<usize, usize> = BTreeMap::new();
    for window in &prepared.windows {
        *lexical.entry(window.file).or_insert(0) += window.rank;
    }
    let mut lexical: Vec<(usize, usize)> = lexical
        .into_iter()
        .filter(|(file, total)| {
            *total > 0
                && !accepted_windows.is_empty()
                && !strength.contains_key(file)
                && !stale.contains(file)
                && !callee_files.contains_key(file)
        })
        .map(|(file, total)| (total, file))
        .collect();
    lexical.sort_by(|a, b| b.cmp(a));
    for (position, &(_, file)) in lexical.iter().take(DEEPEN_LEXICAL_FILES).enumerate() {
        let unread: Vec<usize> = (0..prepared.windows.len())
            .filter(|index| {
                prepared.windows[*index].file == file && !probabilities.contains_key(index)
            })
            .collect();
        if unread.len() < DEEPEN_MIN_UNREAD {
            continue;
        }
        let anchor = (0..prepared.windows.len())
            .filter(|index| prepared.windows[*index].file == file)
            .max_by_key(|index| (prepared.windows[*index].rank, usize::MAX - *index))
            .map(|index| &prepared.windows[index])
            .expect("file has windows");
        for index in unread {
            let window = &prepared.windows[index];
            let distance = window
                .start_line
                .saturating_sub(anchor.end_line)
                .max(anchor.start_line.saturating_sub(window.end_line));
            candidates.push((3000 + position, distance, index));
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
