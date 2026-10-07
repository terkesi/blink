use super::*;
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Mutex, atomic::AtomicUsize},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct Reply {
    status: u16,
    body: Value,
    delay: Duration,
    retry_after: Option<&'static str>,
}
impl Reply {
    fn scores(request: &Value, probability: f64) -> Self {
        let answers: Vec<_> = request["questions"].as_array().unwrap().iter().rev().map(|question| json!({"type": "predicate", "name": question["name"], "probability": probability})).collect();
        Self {
            status: 200,
            body: json!({"answers": answers}),
            delay: Duration::ZERO,
            retry_after: None,
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            body: json!({"private": "never print provider errors"}),
            delay: Duration::ZERO,
            retry_after: None,
        }
    }
}
struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    peak: Arc<AtomicUsize>,
    disconnected: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(answer: impl Fn(&Value, usize) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/decisions", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let peak = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let disconnected = Arc::new(AtomicUsize::new(0));
        let state = (
            Arc::clone(&requests),
            Arc::clone(&peak),
            active,
            Arc::clone(&disconnected),
            Arc::new(answer),
        );
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (requests, peak, active, disconnected, answer) = state.clone();
                tokio::spawn(async move {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    let mut request = Vec::new();
                    let body = loop {
                        let mut chunk = [0; 8192];
                        let size = socket.read(&mut chunk).await.unwrap_or(0);
                        if size == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..size]);
                        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        {
                            let headers =
                                String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                            let length: usize = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length: "))
                                .unwrap()
                                .parse()
                                .unwrap();
                            if request.len() >= end + 4 + length {
                                break request[end + 4..end + 4 + length].to_vec();
                            }
                        }
                    };
                    let value = serde_json::from_slice(&body).unwrap();
                    let number = {
                        let mut requests = requests.lock().unwrap();
                        requests.push(body);
                        requests.len()
                    };
                    let reply = answer(&value, number);
                    let mut probe = [0; 1];
                    tokio::select! {
                        _ = tokio::time::sleep(reply.delay) => {
                            let body = serde_json::to_vec(&reply.body).unwrap();
                            let retry = reply.retry_after.map(|delay| format!("Retry-After: {delay}\r\n")).unwrap_or_default();
                            let header = format!("HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n{retry}\r\n", reply.status, body.len());
                            let _ = socket.write_all(header.as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                            let _ = socket.shutdown().await;
                        }
                        _ = socket.read(&mut probe) => { disconnected.fetch_add(1, Ordering::SeqCst); }
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            endpoint,
            requests,
            peak,
            disconnected,
            task,
        }
    }
    fn provider(&self) -> Provider {
        Provider::for_test(&self.endpoint)
    }
    fn bodies(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}
fn fixture(files: usize) -> TempDir {
    let root = TempDir::new().unwrap();
    for index in 0..files {
        fs::write(
            root.path().join(format!("file{index:03}.rs")),
            format!("fn item_{index}() {{}}\n"),
        )
        .unwrap();
    }
    root
}
async fn run(root: &TempDir, server: &Server, options: Options) -> Report {
    execute(
        Source::open(root.path()).unwrap(),
        "item behavior".into(),
        options,
        server.provider(),
        Arc::new(AtomicBool::new(false)),
    )
    .await
}

#[test]
fn plans_every_byte_with_utf8_boundaries_line_caps_and_overlap() {
    let root = fixture(0);
    let text = format!(
        "{}\n{}",
        "🦀".repeat(3000),
        (0..240).map(|n| format!("line{n}\n")).collect::<String>()
    );
    fs::write(root.path().join("source.rs"), &text).unwrap();
    let prepared = prepare(
        &Source::open(root.path()).unwrap(),
        "line",
        &Options::default(),
        &mut || Control::Continue,
    );
    let mut covered = vec![false; text.len()];
    for window in &prepared.windows {
        assert!(text.is_char_boundary(window.start));
        assert!(text.is_char_boundary(window.end));
        assert!(window.end - window.start <= 4096);
        assert!(window.end_line - window.start_line < 80);
        covered[window.start..window.end].fill(true);
        assert_eq!(
            window.start_line,
            text.as_bytes()[..window.start]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1
        );
        assert_eq!(
            window.end_line,
            text.as_bytes()[..window.end - 1]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1
        );
    }
    assert!(covered.into_iter().all(|byte| byte));
    assert!(
        prepared
            .windows
            .windows(2)
            .all(|pair| pair[0].end_line.saturating_sub(pair[1].start_line) < 8)
    );
}

#[test]
fn exploration_reaches_other_files_and_planning_obeys_control() {
    let root = fixture(40);
    fs::write(
        root.path().join("file000.rs"),
        "relevant query\n".repeat(10_000),
    )
    .unwrap();
    let source = Source::open(root.path()).unwrap();
    let a = prepare(&source, "relevant query", &Options::default(), &mut || {
        Control::Continue
    });
    let b = prepare(&source, "relevant query", &Options::default(), &mut || {
        Control::Continue
    });
    assert_eq!(a.selected, b.selected);
    let files: BTreeSet<_> = a
        .selected
        .iter()
        .step_by(2)
        .map(|&index| a.windows[index].file)
        .collect();
    assert_eq!(files.len(), 32);
    assert!(
        a.selected
            .iter()
            .skip(1)
            .step_by(2)
            .all(|&index| a.windows[index].file == 0)
    );
    let cancelled = prepare(&source, "query", &Options::default(), &mut || {
        Control::Cancel
    });
    assert!(!cancelled.coverage().complete);
    assert_eq!(cancelled.window_count(), 0);
}

#[tokio::test]
async fn real_search_honors_ignores_relative_payloads_exact_ranges_and_answer_names() {
    let root = fixture(0);
    fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(root.path().join("ignored.rs"), "private ignored source").unwrap();
    fs::write(root.path().join(".env"), "private secret").unwrap();
    let text = "fn scheduleRenewal() { /* résumé */ }\n".repeat(180);
    fs::write(root.path().join("scheduler.rs"), &text).unwrap();
    let server = Server::new(|request, _| Reply::scores(request, 0.9)).await;
    let report = run(&root, &server, Options::default()).await;
    assert_eq!(report.exit_code(), 0);
    assert!(report.coverage.complete);
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].excerpt, text);
    assert_eq!(report.results[0].start_byte, 0);
    assert_eq!(report.results[0].end_byte, text.len());
    assert_eq!(report.raw_judgments.len(), report.coverage.windows_planned);
    for body in server.bodies() {
        let request: Value = serde_json::from_slice(&body).unwrap();
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        for candidate in input["candidates"].as_array().unwrap() {
            assert_eq!(candidate["path"], "scheduler.rs");
        }
        let body = String::from_utf8(body).unwrap();
        assert!(!body.contains(root.path().to_str().unwrap()));
        assert!(!body.contains("private"));
    }
}

#[tokio::test]
async fn fully_judged_empty_is_one_but_candidate_bounded_empty_is_three() {
    let server = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let complete = run(&fixture(4), &server, Options::default()).await;
    assert_eq!(complete.exit_code(), 1);
    assert!(complete.coverage.complete);
    assert_eq!(complete.raw_judgments.len(), 4);
    let bounded = run(&fixture(80), &server, Options::default()).await;
    assert_eq!(bounded.exit_code(), 3);
    assert_eq!(bounded.operation, Operation::Completed);
    assert_eq!(bounded.coverage.windows_judged, 64);
    assert_eq!(bounded.coverage.windows_unjudged, 16);
    assert_eq!(bounded.budgets.attempts, 8);
}

#[tokio::test]
async fn reserves_actual_encoded_bytes_and_caps_concurrent_attempts() {
    let root = fixture(80);
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.9);
        reply.delay = Duration::from_millis(25);
        reply
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    assert_eq!(report.budgets.attempts, 8);
    assert_eq!(
        report.budgets.encoded_request_bytes,
        server.bodies().iter().map(Vec::len).sum::<usize>()
    );
    assert_eq!(server.peak.load(Ordering::SeqCst), 4);
    assert!(report.budgets.encoded_request_bytes <= 256 * 1024);
    let no_bytes = execute_with_policy(
        Source::open(root.path()).unwrap(),
        "query".into(),
        Options::default(),
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        Policy {
            max_attempts: 8,
            max_bytes: 1,
            attempt_timeout: Duration::from_secs(1),
        },
    )
    .await;
    assert_eq!(no_bytes.budgets.attempts, 0);
    assert_eq!(no_bytes.budgets.encoded_request_bytes, 0);
    assert_eq!(no_bytes.exit_code(), 3);
    assert!(no_bytes.budgets.stops.contains(&"request_byte_limit"));
}

#[tokio::test]
async fn retry_is_once_and_charges_the_same_encoded_body_again() {
    let server = Server::new(|request, number| {
        if number == 1 {
            Reply::status(503)
        } else {
            Reply::scores(request, 0.9)
        }
    })
    .await;
    let report = run(&fixture(1), &server, Options::default()).await;
    assert_eq!(report.exit_code(), 0);
    assert_eq!(report.budgets.attempts, 2);
    assert_eq!(report.budgets.retries, 1);
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(
        report.budgets.encoded_request_bytes,
        bodies.iter().map(Vec::len).sum::<usize>()
    );
    let failed = Server::new(|_, _| Reply::status(503)).await;
    let report = run(&fixture(1), &failed, Options::default()).await;
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.budgets.attempts, 2);
    assert_eq!(report.errors.len(), 1);
}

#[tokio::test]
async fn retry_cannot_escape_attempt_or_byte_reservations() {
    let root = fixture(1);
    let server = Server::new(|_, _| Reply::status(503)).await;
    let options = Options::default();
    let report = execute_with_policy(
        Source::open(root.path()).unwrap(),
        "query".into(),
        options,
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        Policy {
            max_attempts: 1,
            max_bytes: 256 * 1024,
            attempt_timeout: Duration::from_secs(1),
        },
    )
    .await;
    assert_eq!(report.budgets.attempts, 1);
    assert_eq!(server.bodies().len(), 1);
    assert_eq!(report.exit_code(), 3);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.code == "retry_budget")
    );
}

#[tokio::test]
async fn independent_attempt_deadlines_retry_without_waiting_for_other_requests() {
    let server = Server::new(|request, number| {
        let mut reply = Reply::scores(request, 0.9);
        if number == 1 {
            reply.delay = Duration::from_secs(2);
        }
        reply
    })
    .await;
    let report = execute_with_policy(
        Source::open(fixture(16).path()).unwrap(),
        "query".into(),
        Options::default(),
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        Policy {
            max_attempts: 8,
            max_bytes: 256 * 1024,
            attempt_timeout: Duration::from_millis(50),
        },
    )
    .await;
    assert_eq!(report.budgets.attempts, 3);
    assert_eq!(report.budgets.retries, 1);
    assert_eq!(report.coverage.windows_judged, 16);
}

#[tokio::test]
async fn global_deadline_aborts_requests_and_cancellation_returns_130() {
    let root = fixture(16);
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.9);
        reply.delay = Duration::from_secs(10);
        reply
    })
    .await;
    let report = run(
        &root,
        &server,
        Options {
            timeout: Duration::from_millis(50),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(report.exit_code(), 3);
    assert!(report.budgets.stops.contains(&"deadline"));
    assert!(report.budgets.elapsed_ms < 500);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.disconnected.load(Ordering::SeqCst), 2);
    let cancelled = Arc::new(AtomicBool::new(false));
    let toggle = Arc::clone(&cancelled);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        toggle.store(true, Ordering::Relaxed);
    });
    let report = execute(
        Source::open(root.path()).unwrap(),
        "query".into(),
        Options::default(),
        server.provider(),
        cancelled,
    )
    .await;
    assert_eq!(report.exit_code(), 130);
    assert_eq!(report.operation, Operation::Cancelled);
}

#[tokio::test]
async fn long_retry_delay_is_not_dispatched_past_deadline() {
    let server = Server::new(|_, _| {
        let mut reply = Reply::status(429);
        reply.retry_after = Some("5");
        reply
    })
    .await;
    let report = run(
        &fixture(1),
        &server,
        Options {
            timeout: Duration::from_millis(100),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(report.budgets.attempts, 1);
    assert_eq!(report.exit_code(), 3);
    assert_eq!(server.bodies().len(), 1);
}

#[tokio::test]
async fn changed_source_is_omitted_and_refusal_is_unjudged() {
    let root = fixture(1);
    let path = root.path().join("file000.rs");
    let server = Server::new(move |request, _| {
        fs::write(&path, "changed source").unwrap();
        Reply::scores(request, 0.9)
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    assert_eq!(report.exit_code(), 3);
    assert!(report.results.is_empty());
    assert_eq!(report.changed_files, ["file000.rs"]);
    let server = Server::new(|request, _| { let mut reply = Reply::scores(request, 0.9); reply.body["answers"][0] = json!({"type": "refusal", "name": request["questions"][0]["name"], "reason": "safe refusal"}); reply }).await;
    let report = run(&fixture(1), &server, Options::default()).await;
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.coverage.windows_refused, 1);
    assert_eq!(report.coverage.windows_judged, 0);
}

#[tokio::test]
async fn malformed_answers_are_errors_and_do_not_leak_provider_text() {
    let server = Server::new(|_, _| Reply { status: 200, body: json!({"answers": [{"name": "wrong", "type": "predicate", "probability": 1.0}], "secret": "never print provider errors"}), delay: Duration::ZERO, retry_after: None }).await;
    let mut report = run(&fixture(1), &server, Options::default()).await;
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.budgets.attempts, 1);
    assert_eq!(report.coverage.windows_judged, 0);
    assert!(
        !String::from_utf8(report.encode_json().unwrap())
            .unwrap()
            .contains("never print")
    );
}

#[tokio::test]
async fn encoded_output_limit_preserves_records_and_never_calls_omitted_matches_empty() {
    let root = fixture(0);
    fs::write(root.path().join("huge.rs"), "\"\\\n".repeat(12_000)).unwrap();
    let server = Server::new(|request, _| Reply::scores(request, 0.9)).await;
    let mut report = run(&root, &server, Options::thorough()).await;
    assert_eq!(report.exit_code(), 0);
    assert!(!report.results.is_empty());
    let bytes = report.encode_json().unwrap();
    assert!(bytes.len() <= MAX_OUTPUT_BYTES);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["output_truncated"], true);
    assert!(value["results"].as_array().unwrap().is_empty());
    assert_eq!(report.exit_code(), 3);
    assert!(report.omitted_results > 0);
}

#[test]
fn reservation_arithmetic_rejects_overflow_and_range_checks_reject_empty_source() {
    assert!(
        reserve(
            Reservation {
                attempts: usize::MAX,
                bytes: 0
            },
            1,
            usize::MAX,
            usize::MAX
        )
        .is_none()
    );
    assert!(
        reserve(
            Reservation {
                attempts: 0,
                bytes: usize::MAX
            },
            1,
            usize::MAX,
            usize::MAX
        )
        .is_none()
    );
    assert!(reserve(Reservation::default(), 4, 1, 4).is_some());
    assert!(!valid_range(0, 0, 0));
    assert!(!valid_range(4, 3, 5));
    assert!(!valid_range(0, 6, 5));
    assert!(valid_range(0, 5, 5));
}

#[tokio::test]
async fn retry_bytes_are_reserved_before_the_retry_can_reach_the_server() {
    let root = fixture(1);
    let baseline = Server::new(|request, _| Reply::scores(request, 0.9)).await;
    let first = run(&root, &baseline, Options::default()).await;
    let server = Server::new(|_, _| Reply::status(503)).await;
    let report = execute_with_policy(
        Source::open(root.path()).unwrap(),
        "item behavior".into(),
        Options::default(),
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        Policy {
            max_attempts: 8,
            max_bytes: first.budgets.encoded_request_bytes,
            attempt_timeout: Duration::from_secs(1),
        },
    )
    .await;
    assert_eq!(report.budgets.attempts, 1);
    assert_eq!(server.bodies().len(), 1);
    assert_eq!(
        report.budgets.encoded_request_bytes,
        first.budgets.encoded_request_bytes
    );
    assert!(report.budgets.stops.contains(&"request_byte_limit"));
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.code == "retry_budget")
    );
    assert_eq!(report.exit_code(), 3);
}

#[cfg(unix)]
#[tokio::test]
async fn a_replaced_selected_parent_never_returns_outside_source() {
    let root = fixture(0);
    let outside = fixture(0);
    fs::create_dir(root.path().join("nested")).unwrap();
    fs::write(root.path().join("nested/source.rs"), "fn selected() {}\n").unwrap();
    fs::write(outside.path().join("source.rs"), "outside private source").unwrap();
    let nested = root.path().join("nested");
    let original = root.path().join("original");
    let outside_path = outside.path().to_path_buf();
    let server = Server::new(move |request, _| {
        fs::rename(&nested, &original).unwrap();
        std::os::unix::fs::symlink(&outside_path, &nested).unwrap();
        Reply::scores(request, 0.9)
    })
    .await;
    #[cfg(unix)]
    {
        let mut report = run(&root, &server, Options::default()).await;
        assert_eq!(report.exit_code(), 3);
        assert!(report.results.is_empty());
        let encoded = String::from_utf8(report.encode_json().unwrap()).unwrap();
        assert!(!encoded.contains("outside private source"));
        assert!(!encoded.contains(outside.path().to_str().unwrap()));
    }
}

#[tokio::test]
async fn thorough_policy_judges_more_than_the_default_candidate_quota() {
    let server = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let report = run(&fixture(80), &server, Options::thorough()).await;
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.coverage.windows_judged, 80);
    assert_eq!(report.budgets.attempts, 10);
    assert_eq!(report.budgets.max_attempts, 32);
    assert_eq!(report.budgets.max_encoded_request_bytes, 1024 * 1024);
    assert!(report.coverage.complete);
}

#[tokio::test]
async fn metadata_truncation_retains_valid_json_and_reports_omissions() {
    let server = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let mut report = run(&fixture(0), &server, Options::default()).await;
    report.coverage.source.complete = false;
    report.coverage.complete = false;
    report.operation = Operation::Incomplete;
    report.coverage.source.issues = (0..5000)
        .map(|index| crate::source::Issue {
            path: format!("nested/directory/source{index}.rs"),
            operation: "open",
            kind: "PermissionDenied".into(),
        })
        .collect();
    let bytes = report.encode_json().unwrap();
    assert!(bytes.len() <= MAX_OUTPUT_BYTES);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["output_truncated"], true);
    assert!(report.omitted_metadata_records > 0);
    assert_eq!(
        report.omitted_metadata_records + report.coverage.source.issues.len(),
        5000
    );
    assert_eq!(report.exit_code(), 3);
}

#[tokio::test]
async fn deadline_retains_exact_source_verified_when_an_early_batch_completes() {
    let root = fixture(16);
    let server = Server::new(|request, number| {
        let mut reply = Reply::scores(request, 0.9);
        if number > 1 {
            reply.delay = Duration::from_secs(10);
        }
        reply
    })
    .await;
    let report = run(
        &root,
        &server,
        Options {
            timeout: Duration::from_millis(100),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.operation, Operation::Incomplete);
    assert!(report.budgets.stops.contains(&"deadline"));
    assert_eq!(report.coverage.windows_judged, 8);
    assert_eq!(report.coverage.windows_unjudged, 8);
    assert_eq!(report.results.len(), 8);
    assert!(
        report
            .errors
            .iter()
            .all(|error| error.code != "source_recheck")
    );
    for result in &report.results {
        let source = fs::read_to_string(root.path().join(&result.path)).unwrap();
        assert_eq!(result.excerpt, source[result.start_byte..result.end_byte]);
        assert_eq!(result.start_byte, 0);
        assert_eq!(result.end_byte, source.len());
        assert_eq!(result.start_line, 1);
        assert_eq!(result.end_line, 1);
        assert_eq!(result.sha256.len(), 64);
    }
    assert_eq!(
        report.coverage.source.bytes_read,
        report.coverage.source.bytes_included
            + report
                .results
                .iter()
                .map(|result| result.excerpt.len())
                .sum::<usize>()
    );
}
