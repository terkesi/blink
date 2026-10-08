use super::*;

pub(super) fn recheck(
    prepared: &mut Prepared,
    file: usize,
    fresh: &mut BTreeSet<usize>,
    changed: &mut Vec<String>,
    errors: &mut Vec<SearchError>,
    control: &mut dyn FnMut() -> Control,
) -> bool {
    match prepared.snapshot.recheck(file, control) {
        Ok(true) => true,
        Err(_) if control() != Control::Continue => false,
        result => {
            fresh.remove(&file);
            match result {
                Ok(false) => changed.push(prepared.snapshot.files()[file].path().into()),
                Err(_) => errors.push(SearchError {
                    code: "source_recheck",
                    status: None,
                }),
                Ok(true) => unreachable!(),
            }
            false
        }
    }
}

fn identifiers(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| word.chars().count() >= 4 && word.chars().any(char::is_alphabetic))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    prepared: &Prepared,
    query: &str,
    initial: &BTreeMap<usize, Option<f64>>,
    fresh: &BTreeSet<usize>,
    threshold: f64,
    mut planned: Reservation,
    policy: Policy,
    control: &mut dyn FnMut() -> Control,
) -> (VecDeque<Job>, Option<&'static str>) {
    let text = |index: usize| {
        let window = &prepared.windows[index];
        &prepared.snapshot.files()[window.file].text()[window.start..window.end]
    };
    let donors: Vec<_> = initial
        .iter()
        .filter_map(|(&index, &p)| {
            p.filter(|&p| p >= threshold && fresh.contains(&prepared.windows[index].file))
                .map(|p| (index, p, identifiers(text(index))))
        })
        .collect();
    if donors.is_empty() {
        return (VecDeque::new(), None);
    }
    let mut groups: BTreeMap<usize, Vec<(usize, usize, usize)>> = BTreeMap::new();
    for target in 0..prepared.windows.len() {
        if control() != Control::Continue {
            break;
        }
        let words = identifiers(text(target));
        let donor = donors
            .iter()
            .filter(|(index, _, _)| *index != target)
            .filter_map(|(index, probability, donor_words)| {
                let shared: Vec<_> = words.intersection(donor_words).collect();
                let longest = shared.iter().map(|word| word.chars().count()).max()?;
                let total: usize = shared.iter().map(|word| word.len()).sum();
                Some((*index, *probability, longest, total))
            })
            .max_by(|a, b| {
                a.2.cmp(&b.2)
                    .then(a.3.cmp(&b.3))
                    .then(a.1.total_cmp(&b.1))
                    .then(b.0.cmp(&a.0))
            });
        if let Some((donor, _, longest, total)) = donor {
            groups
                .entry(donor)
                .or_default()
                .push((target, longest, total));
        }
    }
    let capacity = policy.max_attempts.saturating_sub(planned.attempts);
    let mut planned_jobs = Vec::new();
    let ready = tokio::time::Instant::now();
    let mut omitted = false;
    for (donor, mut targets) in groups {
        targets.sort_by_key(|&(target, longest, total)| {
            (
                std::cmp::Reverse(longest),
                std::cmp::Reverse(total),
                initial.get(&target).is_some_and(Option::is_some),
                target,
            )
        });
        let window = &prepared.windows[donor];
        let file = &prepared.snapshot.files()[window.file];
        let donor_name = format!("w{donor}");
        let evidence = Candidate {
            name: &donor_name,
            path: file.path(),
            text: text(donor),
            start_line: window.start_line,
            end_line: window.end_line,
        };
        let mut remaining = targets.as_slice();
        while !remaining.is_empty() {
            if control() != Control::Continue {
                return (VecDeque::new(), None);
            }
            let mut count = remaining.len().min(BATCH_SIZE);
            let fitting = loop {
                let names: Vec<_> = remaining[..count]
                    .iter()
                    .map(|(index, _, _)| format!("w{index}"))
                    .collect();
                let candidates: Vec<_> = remaining[..count]
                    .iter()
                    .zip(&names)
                    .map(|(&(index, _, _), name)| {
                        let window = &prepared.windows[index];
                        Candidate {
                            name,
                            path: prepared.snapshot.files()[window.file].path(),
                            text: text(index),
                            start_line: window.start_line,
                            end_line: window.end_line,
                        }
                    })
                    .collect();
                if let Ok(Some(contextual)) =
                    Batch::encode_with_context(query, &candidates, &evidence, 4096)
                {
                    let contextual_len = contextual.encoded_len();
                    let batch = if INCLUDE_RELATED_EVIDENCE {
                        contextual
                    } else {
                        Batch::encode_related_control(query, &candidates)
                            .expect("validated candidates")
                    };
                    break Some((batch, contextual_len));
                }
                count -= 1;
                if count == 0 {
                    break None;
                }
            };
            let Some((batch, contextual_len)) = fitting else {
                break;
            };
            let (_, longest, total) = remaining[0];
            let targets: Vec<_> = remaining[..count]
                .iter()
                .map(|&(index, _, _)| index)
                .collect();
            let new_count = targets
                .iter()
                .filter(|target| !initial.get(target).is_some_and(Option::is_some))
                .count();
            let order = (
                std::cmp::Reverse(longest),
                std::cmp::Reverse(total),
                std::cmp::Reverse(count),
                std::cmp::Reverse(new_count),
                *targets.iter().min().expect("nonempty targets"),
                donor,
            );
            planned_jobs.push((
                order,
                contextual_len,
                Job {
                    batch,
                    purpose: Purpose::Related { targets, donor },
                    pending_retry: None,
                    refusal_retry: false,
                    ready,
                },
            ));
            planned_jobs.sort_by_key(|(order, _, _)| *order);
            if planned_jobs.len() > capacity {
                planned_jobs.pop();
                omitted = true;
            }
            remaining = &remaining[count..];
        }
    }
    planned_jobs.sort_by_key(|(order, _, _)| *order);
    let mut jobs = VecDeque::new();
    for (_, bytes, job) in planned_jobs {
        let Some(next) = reserve(planned, bytes, policy.max_attempts, policy.max_bytes) else {
            return (
                jobs,
                Some(if planned.attempts == policy.max_attempts {
                    "attempt_limit"
                } else {
                    "request_byte_limit"
                }),
            );
        };
        planned = next;
        jobs.push_back(job);
    }
    (jobs, omitted.then_some("attempt_limit"))
}
