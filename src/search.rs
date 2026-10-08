mod bounds;
mod callees;
mod navigation;
mod related;

use crate::{
    provider::{Batch, Candidate, Failure, Judgment, Provider},
    source::{Control, Coverage, Limits, Snapshot, Source},
};
use bounds::{Reservation, reserve, valid_range};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const BATCH_SIZE: usize = 8;
const CONCURRENCY: usize = 4;
const CALLEE_JOBS: usize = 2;
const CALLEE_BYTES: usize = 96 * 1024;
const INCLUDE_RELATED_EVIDENCE: bool = true;

#[derive(Clone, Debug)]
pub struct Options {
    pub thorough: bool,
    pub limit: usize,
    pub timeout: Duration,
    pub threshold: f64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            thorough: false,
            limit: 8,
            timeout: Duration::from_secs(15),
            threshold: 0.5,
        }
    }
}
impl Options {
    pub fn thorough() -> Self {
        Self {
            thorough: true,
            timeout: Duration::from_secs(60),
            ..Self::default()
        }
    }
    pub fn validate(&self, query: &str) -> Result<(), &'static str> {
        if query.trim().is_empty() || query.len() > 4096 {
            return Err("query must contain 1 to 4096 bytes of nonblank text");
        }
        if !(1..=100).contains(&self.limit) {
            return Err("limit must be between 1 and 100");
        }
        if self.timeout.is_zero() || self.timeout > Duration::from_secs(300) {
            return Err("timeout must be greater than zero and at most 300 seconds");
        }
        if !self.threshold.is_finite() || !(0.0..=1.0).contains(&self.threshold) {
            return Err("threshold must be finite and between zero and one");
        }
        Ok(())
    }
    fn policy(&self) -> Policy {
        if self.thorough {
            Policy {
                max_attempts: 32,
                max_bytes: 1024 * 1024,
                attempt_timeout: Duration::from_secs(15),
            }
        } else {
            Policy {
                max_attempts: 8,
                max_bytes: 256 * 1024,
                attempt_timeout: Duration::from_secs(5),
            }
        }
    }
}
#[derive(Clone, Copy)]
struct Policy {
    max_attempts: usize,
    max_bytes: usize,
    attempt_timeout: Duration,
}

#[derive(Clone, Debug)]
struct Window {
    file: usize,
    start: usize,
    end: usize,
    start_line: usize,
    end_line: usize,
    rank: usize,
}

pub struct Prepared {
    snapshot: Snapshot,
    windows: Vec<Window>,
    selected: Vec<usize>,
    planning_complete: bool,
}
impl Prepared {
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }
    pub fn candidate_count(&self) -> usize {
        self.selected.len()
    }
    pub fn coverage(&self) -> &Coverage {
        self.snapshot.coverage()
    }
}

pub fn prepare(
    source: &Source,
    query: &str,
    options: &Options,
    control: &mut dyn FnMut() -> Control,
) -> Prepared {
    let snapshot = source.snapshot(Limits::default(), control);
    let terms = terms(query);
    let mut windows = Vec::new();
    let mut by_directory = BTreeMap::new();
    let mut planning_complete = true;
    'files: for (file_index, file) in snapshot.files().iter().enumerate() {
        let mut start = 0;
        let first = windows.len();
        let text = file.text();
        let lines = file.line_offsets();
        let path = file.path().to_lowercase();
        while start < text.len() {
            if control() != Control::Continue {
                planning_complete = false;
                break 'files;
            }
            let start_line = lines.partition_point(|&offset| offset <= start);
            let line_end = lines
                .get(start_line - 1 + 80)
                .copied()
                .unwrap_or(text.len());
            let mut end = line_end.min(start.saturating_add(4096)).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            assert!(valid_range(start, end, text.len()));
            let end_line = lines.partition_point(|&offset| offset < end);
            let body = text[start..end].to_lowercase();
            let rank = terms
                .iter()
                .map(|term| {
                    usize::from(path.contains(term)) * 4 + body.matches(term).count().min(8)
                })
                .sum();
            windows.push(Window {
                file: file_index,
                start,
                end,
                start_line,
                end_line,
                rank,
            });
            if end == text.len() {
                break;
            }
            let overlap_line = end_line.saturating_sub(8).max(start_line);
            let overlap = lines.get(overlap_line).copied().unwrap_or(end);
            start = if overlap > start && overlap < end {
                overlap
            } else {
                end
            };
        }
        if first == windows.len() {
            continue;
        }
        let parent = file
            .path()
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);
        by_directory
            .entry(parent)
            .or_insert_with(VecDeque::new)
            .push_back((first, windows.len()));
    }
    let mut directories: VecDeque<_> = by_directory.into_values().collect();
    let mut by_file = Vec::new();
    while let Some(mut files) = directories.pop_front() {
        if control() != Control::Continue {
            planning_complete = false;
            break;
        }
        if let Some(range) = files.pop_front() {
            by_file.push(range);
        }
        if !files.is_empty() {
            directories.push_back(files);
        }
    }
    let slots = options.policy().max_attempts * BATCH_SIZE;
    if windows.len() <= slots {
        by_file.sort_unstable_by_key(|&(first, _)| first);
    }
    let selected = select(&windows, &by_file, slots, control, &mut planning_complete);
    Prepared {
        snapshot,
        windows,
        selected,
        planning_complete,
    }
}

fn terms(query: &str) -> Vec<String> {
    let mut expanded = String::new();
    let mut lower = false;
    for ch in query.chars() {
        if lower && ch.is_uppercase() {
            expanded.push(' ');
        }
        expanded.push(ch);
        lower = ch.is_lowercase();
    }
    expanded
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| term.len() > 1)
        .map(str::to_lowercase)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn select(
    windows: &[Window],
    by_file: &[(usize, usize)],
    slots: usize,
    control: &mut dyn FnMut() -> Control,
    complete: &mut bool,
) -> Vec<usize> {
    let mut best = BinaryHeap::new();
    for (index, window) in windows.iter().enumerate() {
        if control() != Control::Continue {
            *complete = false;
            break;
        }
        let key = (window.rank, std::cmp::Reverse(index));
        if best.len() < slots {
            best.push(std::cmp::Reverse(key));
        } else if best.peek().is_some_and(|worst| key > worst.0) {
            best.pop();
            best.push(std::cmp::Reverse(key));
        }
    }
    let mut ranked: Vec<_> = best.into_iter().map(|key| key.0.1.0).collect();
    ranked.sort_unstable_by_key(|&index| (std::cmp::Reverse(windows[index].rank), index));
    let mut files: VecDeque<_> = by_file
        .iter()
        .copied()
        .filter(|&(start, end)| start < end)
        .collect();
    let mut picked = BTreeSet::new();
    let mut selected = Vec::new();
    let mut ranked = ranked.into_iter();
    while selected.len() < slots.min(windows.len()) {
        if control() != Control::Continue {
            *complete = false;
            break;
        }
        let exploration = selected.len() % 2 == 0;
        let next = if exploration {
            loop {
                let Some((mut index, end)) = files.pop_front() else {
                    break None;
                };
                while index < end && picked.contains(&index) {
                    index += 1;
                }
                if index < end {
                    if index + 1 < end {
                        files.push_back((index + 1, end));
                    }
                    break Some(index);
                }
            }
        } else {
            None
        };
        let next = next.or_else(|| ranked.find(|index| !picked.contains(index)));
        let Some(index) = next else {
            break;
        };
        picked.insert(index);
        selected.push(index);
    }
    selected
}

#[derive(Clone, Debug, Serialize)]
pub struct ResultRecord {
    pub path: String,
    pub sha256: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub probability: f64,
    pub excerpt: String,
}
#[derive(Clone, Debug)]
pub struct RawJudgment {
    pub name: String,
    pub path: String,
    pub sha256: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub probability: Option<f64>,
}
#[derive(Clone, Debug)]
pub struct JudgmentEvent {
    pub name: String,
    pub donor: Option<String>,
    pub probability: Option<f64>,
}
#[derive(Debug, Serialize)]
pub struct SearchCoverage {
    pub source: Coverage,
    pub planning_complete: bool,
    pub windows_planned: usize,
    pub windows_selected: usize,
    pub windows_sent: usize,
    pub windows_judged: usize,
    pub windows_refused: usize,
    pub windows_unjudged: usize,
    pub complete: bool,
}
#[derive(Debug, Serialize)]
pub struct Budgets {
    pub attempts: usize,
    pub max_attempts: usize,
    pub encoded_request_bytes: usize,
    pub max_encoded_request_bytes: usize,
    pub retries: usize,
    pub max_concurrent_requests: usize,
    pub elapsed_ms: u128,
    pub timeout_ms: u128,
    pub stops: Vec<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct SearchError {
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Completed,
    Incomplete,
    Cancelled,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub command: &'static str,
    pub operation: Operation,
    pub results: Vec<ResultRecord>,
    pub coverage: SearchCoverage,
    pub budgets: Budgets,
    pub errors: Vec<SearchError>,
    pub changed_files: Vec<String>,
    pub output_truncated: bool,
    pub omitted_results: usize,
    pub omitted_metadata_records: usize,
    #[serde(skip)]
    pub raw_judgments: Vec<RawJudgment>,
    #[serde(skip)]
    pub sent_names: Vec<String>,
    #[serde(skip)]
    pub judgment_events: Vec<JudgmentEvent>,
}
impl Report {
    pub fn exit_code(&self) -> u8 {
        if self.operation == Operation::Cancelled {
            130
        } else if self.operation == Operation::Incomplete {
            3
        } else if !self.results.is_empty() {
            0
        } else if self.coverage.complete && self.omitted_results == 0 {
            1
        } else {
            3
        }
    }
    pub fn encode_json(&mut self) -> Result<Vec<u8>, serde_json::Error> {
        loop {
            let mut bytes = serde_json::to_vec(self)?;
            bytes.push(b'\n');
            if bytes.len() <= MAX_OUTPUT_BYTES {
                return Ok(bytes);
            }
            self.output_truncated = true;
            if self.results.pop().is_some() {
                self.omitted_results += 1;
            } else if !self.coverage.source.issues.is_empty() {
                let keep = self.coverage.source.issues.len() / 2;
                self.omitted_metadata_records += self.coverage.source.issues.len() - keep;
                self.coverage.source.issues.truncate(keep);
            } else if !self.changed_files.is_empty() {
                self.changed_files.pop();
                self.omitted_metadata_records += 1;
            } else if !self.errors.is_empty() {
                self.errors.pop();
                self.omitted_metadata_records += 1;
            } else {
                unreachable!("bounded metadata fits the output limit");
            }
        }
    }
    pub fn encode_text(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut retained = 0;
        for record in &self.results {
            let mut text = format!(
                "{}:{}-{} ({:.3})\n",
                serde_json::to_string(&record.path).expect("path serializes"),
                record.start_line,
                record.end_line,
                record.probability
            );
            for ch in record.excerpt.chars() {
                if ch == '\n' || ch == '\t' || !ch.is_control() {
                    text.push(ch);
                } else {
                    text.extend(ch.escape_default());
                }
            }
            text.push('\n');
            if bytes.len() + text.len() > MAX_OUTPUT_BYTES {
                break;
            }
            bytes.extend_from_slice(text.as_bytes());
            retained += 1;
        }
        self.omitted_results += self.results.len() - retained;
        self.output_truncated |= retained < self.results.len();
        self.results.truncate(retained);
        bytes
    }
}

#[derive(Clone)]
enum Purpose {
    Route(Vec<usize>),
    Source(Vec<usize>),
    Related { targets: Vec<usize>, donor: usize },
}

#[derive(Clone)]
struct Job {
    batch: Batch,
    purpose: Purpose,
    pending_retry: Option<usize>,
    ready: tokio::time::Instant,
}
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Initial,
    Related,
    Callee,
}

enum Attempt {
    Response(Result<Vec<Judgment>, Failure>),
    Deadline,
}

impl Attempt {
    fn stopped_error(&self) -> Option<SearchError> {
        match self {
            Self::Response(Err(error)) => Some(SearchError {
                code: error.code(),
                status: error.status(),
            }),
            Self::Response(Ok(judgments)) if judgments.iter().any(|j| j.probability.is_none()) => {
                Some(SearchError {
                    code: "provider_refusal",
                    status: None,
                })
            }
            Self::Deadline => Some(SearchError {
                code: "attempt_timeout",
                status: None,
            }),
            Self::Response(Ok(_)) => None,
        }
    }
}

pub async fn execute(
    source: Source,
    query: String,
    options: Options,
    provider: Provider,
    cancelled: Arc<AtomicBool>,
) -> Report {
    execute_with_policy(
        source,
        query,
        options.clone(),
        provider,
        cancelled,
        options.policy(),
    )
    .await
}

fn candidate<'a>(prepared: &'a Prepared, index: usize, name: &'a str) -> Candidate<'a> {
    let window = &prepared.windows[index];
    let file = &prepared.snapshot.files()[window.file];
    Candidate {
        name,
        path: file.path(),
        text: &file.text()[window.start..window.end],
        start_line: window.start_line,
        end_line: window.end_line,
    }
}

fn source_batch(prepared: &Prepared, query: &str, indices: &[usize]) -> Result<Batch, Failure> {
    let names: Vec<_> = indices.iter().map(|index| format!("w{index}")).collect();
    let candidates: Vec<_> = indices
        .iter()
        .zip(&names)
        .map(|(&index, name)| candidate(prepared, index, name))
        .collect();
    Batch::encode(query, &candidates)
}

async fn execute_with_policy(
    source: Source,
    query: String,
    options: Options,
    provider: Provider,
    cancelled: Arc<AtomicBool>,
    policy: Policy,
) -> Report {
    let initial_policy = policy;
    let policy = if options.thorough {
        policy
    } else {
        Policy {
            max_attempts: policy.max_attempts * 2,
            max_bytes: policy.max_bytes * 2,
            ..policy
        }
    };
    let followup = if options.thorough {
        policy
    } else {
        Policy {
            max_attempts: policy.max_attempts + CALLEE_JOBS,
            max_bytes: policy.max_bytes + CALLEE_BYTES,
            ..policy
        }
    };
    let actual_policy = if options.thorough {
        policy
    } else {
        Policy {
            max_attempts: followup.max_attempts + 2,
            max_bytes: followup.max_bytes + 128 * 1024,
            ..policy
        }
    };
    let mut phase = Phase::Initial;
    let mut initial_state = None;
    let mut related_sent = false;
    let mut callee_queue = VecDeque::new();
    let started = Instant::now();
    let deadline = started + options.timeout;
    let control = || {
        if cancelled.load(Ordering::Relaxed) {
            Control::Cancel
        } else if Instant::now() >= deadline {
            Control::Deadline
        } else {
            Control::Continue
        }
    };
    let mut prepared = prepare(&source, &query, &options, &mut { &control });
    let mut queue = VecDeque::new();
    let mut errors = Vec::new();
    let mut frontier = navigation::Frontier::new(&prepared, !options.thorough);
    match frontier.routes(&prepared, &query, &mut { &control }) {
        Ok(routes) => queue.extend(routes.into_iter().map(|(batch, ids)| Job {
            batch,
            purpose: Purpose::Route(ids),
            pending_retry: None,
            ready: tokio::time::Instant::now(),
        })),
        Err(error) => errors.push(SearchError {
            code: error.code(),
            status: error.status(),
        }),
    }
    let mut routes_pending = queue.len();
    let mut source_jobs = 0;
    let mut source_exhausted = false;
    let mut selected = if options.thorough {
        prepared.selected.iter().copied().collect()
    } else {
        BTreeSet::new()
    };
    let mut tasks = JoinSet::new();
    let mut used = Reservation::default();
    let mut retries = 0;
    let mut pending_errors = Vec::new();
    let mut sent = BTreeSet::new();
    let mut refusal_retried = BTreeSet::new();
    let mut probabilities = BTreeMap::new();
    let mut judgment_events = Vec::new();
    let mut checked = BTreeSet::new();
    let mut fresh = BTreeSet::new();
    let mut changed_files = Vec::new();
    let mut stops = BTreeSet::new();
    let mut interrupted = false;
    loop {
        match control() {
            Control::Cancel => {
                interrupted = true;
                stops.insert("cancelled");
                break;
            }
            Control::Deadline => {
                stops.insert("deadline");
                break;
            }
            Control::Continue => {}
        }
        let admission = if phase == Phase::Initial {
            initial_policy
        } else {
            followup
        };
        while tasks.len() < CONCURRENCY {
            if control() != Control::Continue {
                break;
            }
            let now = tokio::time::Instant::now();
            let job = if queue.front().is_some_and(|job| job.ready <= now) {
                queue.pop_front().expect("ready job")
            } else if !source_exhausted && (routes_pending == 0 || source_jobs < 2) {
                if used.attempts >= admission.max_attempts {
                    stops.insert("attempt_limit");
                    source_exhausted = true;
                    break;
                }
                let Some(indices) = frontier.next(&mut { &control }) else {
                    source_exhausted = true;
                    break;
                };
                selected.extend(indices.iter().copied());
                source_jobs += 1;
                match source_batch(&prepared, &query, &indices) {
                    Ok(batch) => Job {
                        batch,
                        purpose: Purpose::Source(indices),
                        pending_retry: None,
                        ready: now,
                    },
                    Err(error) => {
                        errors.push(SearchError {
                            code: error.code(),
                            status: error.status(),
                        });
                        continue;
                    }
                }
            } else {
                break;
            };
            if let Purpose::Related { donor, .. } = &job.purpose {
                let file = prepared.windows[*donor].file;
                if !fresh.contains(&file)
                    || !related::recheck(
                        &mut prepared,
                        file,
                        &mut fresh,
                        &mut changed_files,
                        &mut errors,
                        &mut { &control },
                    )
                {
                    continue;
                }
            }
            let admission = if job.pending_retry.is_some() {
                actual_policy
            } else {
                admission
            };
            let Some(reservation) = reserve(
                used,
                job.batch.encoded_len(),
                admission.max_attempts,
                admission.max_bytes,
            ) else {
                stops.insert(if used.attempts >= admission.max_attempts {
                    "attempt_limit"
                } else {
                    "request_byte_limit"
                });
                if matches!(job.purpose, Purpose::Route(_)) {
                    routes_pending -= 1;
                }
                if job.pending_retry.is_some() {
                    errors.push(SearchError {
                        code: "retry_budget",
                        status: None,
                    });
                }
                continue;
            };
            used = reservation;
            related_sent |= matches!(job.purpose, Purpose::Related { .. });
            retries += usize::from(job.pending_retry.is_some());
            match &job.purpose {
                Purpose::Source(indices)
                | Purpose::Related {
                    targets: indices, ..
                } => {
                    selected.extend(indices.iter().copied());
                    sent.extend(indices.iter().copied());
                }
                Purpose::Route(_) => {}
            }
            let provider = provider.clone();
            let attempt_deadline =
                tokio::time::Instant::from_std(deadline).min(now + policy.attempt_timeout);
            tasks.spawn(async move {
                let outcome =
                    match tokio::time::timeout_at(attempt_deadline, provider.attempt(&job.batch))
                        .await
                    {
                        Ok(response) => Attempt::Response(response),
                        Err(_) => Attempt::Deadline,
                    };
                (job, outcome)
            });
        }
        if tasks.is_empty() && queue.is_empty() && source_exhausted {
            if phase == Phase::Initial && !options.thorough {
                phase = Phase::Related;
                initial_state = Some((probabilities.clone(), fresh.clone(), judgment_events.len()));
                let donors: BTreeSet<_> = probabilities
                    .iter()
                    .filter_map(|(&index, probability): (&usize, &Option<f64>)| {
                        probability
                            .filter(|&p| p >= options.threshold)
                            .map(|_| prepared.windows[index].file)
                    })
                    .collect();
                for file in donors {
                    related::recheck(
                        &mut prepared,
                        file,
                        &mut fresh,
                        &mut changed_files,
                        &mut errors,
                        &mut { &control },
                    );
                }
                let (related_jobs, stop) = related::plan(
                    &prepared,
                    &query,
                    &probabilities,
                    &fresh,
                    options.threshold,
                    used,
                    policy,
                    &mut { &control },
                );
                callee_queue = callees::plan(
                    &prepared,
                    &query,
                    &probabilities,
                    &fresh,
                    options.threshold,
                    &related_jobs,
                    used,
                    followup,
                    &mut { &control },
                );
                queue = related_jobs;
                if let Some(stop) = stop {
                    stops.insert(stop);
                }
                continue;
            }
            if phase == Phase::Related
                && !callee_queue.is_empty()
                && tokio::time::Instant::now() + policy.attempt_timeout
                    <= tokio::time::Instant::from_std(deadline)
            {
                phase = Phase::Callee;
                initial_state = Some((probabilities.clone(), fresh.clone(), judgment_events.len()));
                queue = std::mem::take(&mut callee_queue);
                queue.retain(|job| {
                    matches!(job.purpose, Purpose::Related { donor, .. }
                        if probabilities.get(&donor).copied().flatten().is_some_and(|p| p >= options.threshold))
                });
                continue;
            }
            break;
        }
        let wake = if tasks.len() < CONCURRENCY {
            queue.front().map(|job| job.ready)
        } else {
            None
        }
        .unwrap_or_else(|| tokio::time::Instant::from_std(deadline));
        let poll = (tokio::time::Instant::now() + Duration::from_millis(10))
            .min(wake)
            .min(tokio::time::Instant::from_std(deadline));
        tokio::select! {
            result = tasks.join_next(), if !tasks.is_empty() => {
                let Some(Ok((mut job, outcome))) = result else {
                    errors.push(SearchError { code: "coordinator_task", status: None });
                    continue;
                };
                if control() != Control::Continue {
                    if let Some(error) = outcome.stopped_error() { errors.push(error); }
                    continue;
                }
                match outcome {
                    Attempt::Response(Ok(judgments)) => {
                        if let Some(index) = job.pending_retry { pending_errors[index] = None; }
                        if let Purpose::Route(ids) = &job.purpose {
                            routes_pending -= 1;
                            frontier.observe(ids, &judgments);
                            if judgments.iter().any(|judgment| judgment.probability.is_none()) {
                                errors.push(SearchError { code: "provider_refusal", status: None });
                            }
                            continue;
                        }
                        let mut refused = Vec::new();
                        for judgment in judgments {
                            let index = judgment.name.strip_prefix('w').and_then(|name| name.parse::<usize>().ok()).expect("provider validates batch names");
                            let donor = match &job.purpose { Purpose::Related { donor, .. } => Some(format!("w{donor}")), _ => None };
                            judgment_events.push(JudgmentEvent { name: judgment.name, donor, probability: judgment.probability });
                            let Some(probability) = judgment.probability else {
                                probabilities.entry(index).or_insert(None);
                                if refusal_retried.insert(index) {
                                    refused.push(index);
                                }
                                continue;
                            };
                            probabilities.insert(index, Some(probability));
                            let file = prepared.windows[index].file;
                            if probability >= options.threshold && checked.insert(file) {
                                match prepared.snapshot.recheck(file, &mut { &control }) {
                                    Ok(true) => { fresh.insert(file); }
                                    Ok(false) => changed_files.push(prepared.snapshot.files()[file].path().into()),
                                    Err(_) => errors.push(SearchError { code: "source_recheck", status: None }),
                                }
                            }
                        }
                        for index in refused {
                            let name = format!("w{index}");
                            let window = candidate(&prepared, index, &name);
                            let (batch, purpose) = match &job.purpose {
                                Purpose::Related { donor, .. } => {
                                    let donor_name = format!("w{donor}");
                                    let evidence = candidate(&prepared, *donor, &donor_name);
                                    (
                                        Batch::encode_with_context(
                                            &query,
                                            std::slice::from_ref(&window),
                                            &evidence,
                                        )
                                        .ok()
                                        .flatten(),
                                        Purpose::Related {
                                            targets: vec![index],
                                            donor: *donor,
                                        },
                                    )
                                }
                                _ => (
                                    Batch::encode(&query, std::slice::from_ref(&window)).ok(),
                                    Purpose::Source(vec![index]),
                                ),
                            };
                            let Some(batch) = batch else { continue };
                            queue.push_back(Job {
                                batch,
                                purpose,
                                pending_retry: None,
                                ready: tokio::time::Instant::now(),
                            });
                            queue.make_contiguous().sort_by_key(|job| job.ready);
                        }
                    }
                    outcome => {
                        let (code, status, retryable, delay) = match outcome {
                            Attempt::Response(Err(error)) => (error.code(), error.status(), error.retryable(), error.retry_after().unwrap_or(Duration::from_millis(100))),
                            Attempt::Deadline => ("attempt_timeout", None, true, Duration::ZERO),
                            Attempt::Response(Ok(_)) => unreachable!(),
                        };
                        let retry_at = tokio::time::Instant::now().checked_add(delay);
                        if retryable && job.pending_retry.is_none() && retry_at.is_some_and(|at| at < tokio::time::Instant::from_std(deadline)) {
                            job.pending_retry = Some(pending_errors.len());
                            pending_errors.push(Some(SearchError { code, status }));
                            job.ready = retry_at.expect("retry delay checked");
                            queue.push_back(job);
                            queue.make_contiguous().sort_by_key(|job| job.ready);
                        } else {
                            if let Some(index) = job.pending_retry { pending_errors[index] = None; }
                            if matches!(job.purpose, Purpose::Route(_)) { routes_pending -= 1; }
                            errors.push(SearchError { code, status });
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(poll) => {}
        }
    }
    errors.extend(pending_errors.into_iter().flatten());
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    if related_sent {
        let accepted_files: BTreeSet<_> = probabilities
            .iter()
            .filter_map(|(&index, probability)| {
                probability
                    .filter(|&p| p >= options.threshold)
                    .map(|_| prepared.windows[index].file)
            })
            .collect();
        for file in accepted_files {
            if control() != Control::Continue {
                break;
            }
            if fresh.contains(&file) {
                related::recheck(
                    &mut prepared,
                    file,
                    &mut fresh,
                    &mut changed_files,
                    &mut errors,
                    &mut { &control },
                );
            }
        }
    }
    if control() != Control::Continue
        && let Some((initial_probabilities, initial_fresh, initial_events)) = initial_state
    {
        probabilities = initial_probabilities;
        fresh.retain(|file| initial_fresh.contains(file));
        judgment_events.truncate(initial_events);
    }
    let raw_judgments = probabilities
        .iter()
        .map(|(&index, &probability)| {
            let window = &prepared.windows[index];
            let file = &prepared.snapshot.files()[window.file];
            RawJudgment {
                name: format!("w{index}"),
                path: file.path().into(),
                sha256: file.sha256().into(),
                start_byte: window.start,
                end_byte: window.end,
                start_line: window.start_line,
                end_line: window.end_line,
                probability,
            }
        })
        .collect();
    let accepted: Vec<_> = probabilities
        .iter()
        .filter_map(|(&index, &probability)| {
            probability
                .filter(|&p| p >= options.threshold)
                .map(|p| (index, p))
        })
        .collect();
    changed_files.sort();
    changed_files.dedup();
    let mut results = merge(&prepared, &accepted, &fresh);
    results.sort_by(|a, b| {
        b.probability
            .total_cmp(&a.probability)
            .then_with(|| a.path.cmp(&b.path))
            .then(a.start_byte.cmp(&b.start_byte))
    });
    let omitted_results = results.len().saturating_sub(options.limit);
    if omitted_results > 0 {
        let mut occurrences = BTreeMap::new();
        let mut rounds: Vec<_> = results
            .into_iter()
            .map(|record| {
                let parent = record
                    .path
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent);
                let occurrence = occurrences.entry(parent.to_owned()).or_insert(0usize);
                let round = *occurrence;
                *occurrence += 1;
                (round, record)
            })
            .collect();
        rounds.sort_by_key(|(round, _)| *round);
        results = rounds.into_iter().map(|(_, record)| record).collect();
    }
    results.truncate(options.limit);
    let judged = probabilities
        .values()
        .filter(|value| value.is_some())
        .count();
    let refused = probabilities.len() - judged;
    if control() == Control::Cancel {
        interrupted = true;
        stops.insert("cancelled");
    }
    if control() == Control::Deadline {
        stops.insert("deadline");
    }
    let coverage_complete = prepared.snapshot.coverage().complete
        && prepared.planning_complete
        && judged == prepared.windows.len()
        && changed_files.is_empty();
    let operation = if interrupted {
        Operation::Cancelled
    } else if stops.contains("deadline")
        || !errors.is_empty()
        || !changed_files.is_empty()
        || !prepared.snapshot.coverage().complete
        || !prepared.planning_complete
    {
        Operation::Incomplete
    } else {
        Operation::Completed
    };
    if selected.len() < prepared.windows.len() {
        stops.insert("candidate_limit");
    }
    let coverage = SearchCoverage {
        planning_complete: prepared.planning_complete,
        windows_planned: prepared.windows.len(),
        windows_selected: selected.len(),
        windows_sent: sent.len(),
        windows_judged: judged,
        windows_refused: refused,
        windows_unjudged: prepared.windows.len() - judged,
        complete: coverage_complete,
        source: prepared.snapshot.into_coverage(),
    };
    Report {
        schema_version: 1,
        command: "search",
        operation,
        results,
        coverage,
        budgets: Budgets {
            attempts: used.attempts,
            max_attempts: actual_policy.max_attempts,
            encoded_request_bytes: used.bytes,
            max_encoded_request_bytes: actual_policy.max_bytes,
            retries,
            max_concurrent_requests: CONCURRENCY,
            elapsed_ms: started.elapsed().as_millis(),
            timeout_ms: options.timeout.as_millis(),
            stops: stops.into_iter().collect(),
        },
        errors,
        changed_files,
        output_truncated: false,
        omitted_results,
        omitted_metadata_records: 0,
        raw_judgments,
        sent_names: sent.iter().map(|index| format!("w{index}")).collect(),
        judgment_events,
    }
}

fn merge(
    prepared: &Prepared,
    accepted: &[(usize, f64)],
    fresh: &BTreeSet<usize>,
) -> Vec<ResultRecord> {
    let mut results: Vec<ResultRecord> = Vec::new();
    for &(index, probability) in accepted {
        let window = &prepared.windows[index];
        if !fresh.contains(&window.file) {
            continue;
        }
        let file = &prepared.snapshot.files()[window.file];
        if let Some(last) = results.last_mut()
            && last.path == file.path()
            && window.start <= last.end_byte
        {
            last.end_byte = last.end_byte.max(window.end);
            last.end_line = last.end_line.max(window.end_line);
            last.probability = last.probability.max(probability);
            last.excerpt = file.text()[last.start_byte..last.end_byte].into();
        } else {
            results.push(ResultRecord {
                path: file.path().into(),
                sha256: file.sha256().into(),
                start_byte: window.start,
                end_byte: window.end,
                start_line: window.start_line,
                end_line: window.end_line,
                probability,
                excerpt: file.text()[window.start..window.end].into(),
            });
        }
    }
    results
}

#[cfg(test)]
mod tests;
