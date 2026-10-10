use super::*;

const DECLARATIONS: &str = "def class fn fun func function struct enum trait interface type const let var \
     val typealias protocol extension actor typedef union record object module mod namespace";
const MODIFIERS: &str =
    "pub export default async static public private protected abstract final unsafe override";

pub(super) fn word(text: &str) -> (&str, &str) {
    text.split_at(
        text.find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(text.len()),
    )
}

pub(super) fn is_modifier(word: &str) -> bool {
    listed(MODIFIERS, word)
}

pub(super) fn skip_group(text: &str) -> &str {
    let text = text.trim_start();
    text.strip_prefix('(').map_or(text, |inner| {
        inner
            .split_once(')')
            .map_or("", |(_, rest)| rest.trim_start())
    })
}

fn listed(list: &str, word: &str) -> bool {
    list.split(' ').any(|item| item == word)
}

pub(super) fn declaration(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    let mut modified = false;
    loop {
        let (keyword, tail) = word(rest);
        let declares = listed(DECLARATIONS, keyword);
        if listed(MODIFIERS, keyword) || declares && listed(DECLARATIONS, word(tail.trim_start()).0)
        {
            rest = skip_group(tail);
            modified = true;
        } else if declares && tail.starts_with(char::is_whitespace) {
            let tail = if keyword == "func" {
                skip_group(tail)
            } else {
                tail.trim_start()
            };
            let (name, after) = match word(tail) {
                ("mut", after) if keyword == "let" => word(after.trim_start()),
                pair => pair,
            };
            let value = listed("let const var val", keyword)
                && !after.split_once('=').is_some_and(|(_, init)| {
                    let (start, rest) = word(init.trim_start());
                    init.contains("=>")
                        || init.trim_start().starts_with('|')
                        || start == "function"
                        || start == "async" && !rest.starts_with('(')
                });
            return Some(name).filter(|name| !name.is_empty() && !value);
        } else {
            return (modified
                && !declares
                && !keyword.is_empty()
                && tail.trim_start().starts_with('('))
            .then_some(keyword);
        }
    }
}

pub(super) fn declarations(text: &str) -> BTreeSet<&str> {
    text.lines().filter_map(declaration).collect()
}

pub(super) fn calls(text: &str) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let declared = declaration(line);
        let mut start = None;
        for (offset, ch) in line.char_indices() {
            if ch.is_alphanumeric() || ch == '_' {
                start.get_or_insert(offset);
            } else if let Some(begin) = start.take()
                && line[offset..]
                    .trim_start_matches([' ', '\t'])
                    .starts_with('(')
                && !line[begin..].starts_with(|c: char| c.is_ascii_digit())
                && declared != Some(&line[begin..offset])
            {
                names.insert(&line[begin..offset]);
            }
        }
    }
    names
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    prepared: &Prepared,
    query: &str,
    initial: &BTreeMap<usize, Option<f64>>,
    fresh: &BTreeSet<usize>,
    threshold: f64,
    planned: &VecDeque<Job>,
    mut reserved: Reservation,
    policy: Policy,
    control: &mut dyn FnMut() -> Control,
) -> VecDeque<Job> {
    let text = |index: usize| {
        let window = &prepared.windows[index];
        &prepared.snapshot.files()[window.file].text()[window.start..window.end]
    };
    let mut excluded = BTreeSet::new();
    for job in planned {
        if let Purpose::Related { targets, .. } = &job.purpose {
            excluded.extend(targets.iter().copied());
        }
        let Some(next) = reserve(
            reserved,
            job.batch.encoded_len(),
            policy.max_attempts,
            policy.max_bytes,
        ) else {
            return VecDeque::new();
        };
        reserved = next;
    }
    let donors: Vec<_> = initial
        .iter()
        .filter_map(|(&index, &p)| {
            let p =
                p.filter(|&p| p >= threshold && fresh.contains(&prepared.windows[index].file))?;
            let name = format!("w{index}");
            let card = candidate(prepared, index, &name);
            matches!(
                Batch::encode_with_context(query, std::slice::from_ref(&card), &card, 4096),
                Ok(Some(_))
            )
            .then(|| (index, p, calls(text(index))))
        })
        .collect();
    if donors.is_empty() {
        return VecDeque::new();
    }
    let mut declared = Vec::with_capacity(prepared.windows.len());
    for index in 0..prepared.windows.len() {
        if control() != Control::Continue {
            return VecDeque::new();
        }
        declared.push(declarations(text(index)));
    }
    let mut files: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
    for (index, names) in declared.iter().enumerate() {
        for &name in names {
            files
                .entry(name)
                .or_default()
                .insert(prepared.windows[index].file);
        }
    }
    let mut groups: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
    for target in 0..prepared.windows.len() {
        if control() != Control::Continue {
            return VecDeque::new();
        }
        if excluded.contains(&target)
            || initial
                .get(&target)
                .is_some_and(|p| p.is_some_and(|p| p >= threshold))
        {
            continue;
        }
        let previous = target
            .checked_sub(1)
            .filter(|&previous| prepared.windows[previous].file == prepared.windows[target].file);
        let defined: Vec<_> = declared[target]
            .iter()
            .chain(
                previous
                    .into_iter()
                    .flat_map(|previous| &declared[previous]),
            )
            .collect();
        let donor = donors
            .iter()
            .filter(|(index, ..)| *index != target)
            .filter_map(|(index, p, calls)| {
                let count = defined
                    .iter()
                    .filter(|name| calls.contains(**name))
                    .map(|name| files[**name].len())
                    .min()?;
                Some((count, *p, *index))
            })
            .min_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
        if let Some((count, _, donor)) = donor {
            groups.entry(donor).or_default().push((count, target));
        }
    }
    let ready = tokio::time::Instant::now();
    let mut jobs = Vec::new();
    for (donor, mut targets) in groups {
        targets.sort_unstable();
        let donor_name = format!("w{donor}");
        let evidence = candidate(prepared, donor, &donor_name);
        let mut remaining = targets.as_slice();
        for _ in 0..CALLEE_JOBS {
            let Some((batch, count)) =
                (1..=remaining.len().min(BATCH_SIZE))
                    .rev()
                    .find_map(|count| {
                        let names: Vec<_> = remaining[..count]
                            .iter()
                            .map(|(_, index)| format!("w{index}"))
                            .collect();
                        let cards: Vec<_> = remaining[..count]
                            .iter()
                            .zip(&names)
                            .map(|(&(_, index), name)| candidate(prepared, index, name))
                            .collect();
                        Batch::encode_with_context(query, &cards, &evidence, 4096)
                            .ok()
                            .flatten()
                            .map(|contextual| {
                                let batch = if INCLUDE_RELATED_EVIDENCE {
                                    contextual
                                } else {
                                    Batch::encode_related_control(query, &cards)
                                        .expect("validated candidates")
                                };
                                (batch, count)
                            })
                    })
            else {
                break;
            };
            let order = (remaining[0], std::cmp::Reverse(count), donor);
            let targets = remaining[..count].iter().map(|&(_, index)| index).collect();
            jobs.push((
                order,
                Job {
                    batch,
                    purpose: Purpose::Related { targets, donor },
                    pending_retry: None,
                    retries_done: 0,
                    refusal_retry: false,
                    ready,
                },
            ));
            remaining = &remaining[count..];
        }
    }
    jobs.sort_by_key(|(order, _)| *order);
    let mut queue = VecDeque::new();
    for (_, job) in jobs.into_iter().take(CALLEE_JOBS) {
        let Some(next) = reserve(
            reserved,
            job.batch.encoded_len(),
            policy.max_attempts,
            policy.max_bytes,
        ) else {
            break;
        };
        reserved = next;
        queue.push_back(job);
    }
    queue
}
