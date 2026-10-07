use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

#[test]
fn exploration_preserves_file_order_when_all_windows_fit() {
    let root = fixture(0);
    for directory in ["a", "b"] {
        fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    for (path, text) in [
        ("a/first.rs", "fn first() {}\n"),
        ("a/second.rs", "fn second() {}\n"),
        ("b/first.rs", "fn first() {}\n"),
        ("b/second.rs", "relevant query\n"),
    ] {
        fs::write(root.path().join(path), text).unwrap();
    }
    let source = Source::open(root.path()).unwrap();
    for options in [Options::default(), Options::thorough()] {
        let prepared = prepare(&source, "relevant query", &options, &mut || {
            Control::Continue
        });
        let paths: Vec<_> = prepared
            .selected
            .iter()
            .map(|&index| prepared.snapshot.files()[prepared.windows[index].file].path())
            .collect();
        assert_eq!(
            paths,
            ["a/first.rs", "b/second.rs", "a/second.rs", "b/first.rs"]
        );
    }
}

#[test]
fn exploration_reaches_distinct_directories_within_the_candidate_cap() {
    let root = fixture(0);
    for directory in ["a", "b/nested", "c"] {
        fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    for index in 0..300 {
        fs::write(
            root.path().join(format!("a/file{index:03}.rs")),
            "fn item() {}\n",
        )
        .unwrap();
    }
    fs::write(
        root.path().join("a/file000.rs"),
        "relevant query\n".repeat(10_000),
    )
    .unwrap();
    for path in ["root.rs", "b/nested/source.rs", "c/source.rs"] {
        fs::write(root.path().join(path), "fn item() {}\n").unwrap();
    }
    for index in 0..40 {
        fs::write(root.path().join(format!("b/nested/empty{index:03}.rs")), "").unwrap();
    }
    let source = Source::open(root.path()).unwrap();
    for (options, cap) in [(Options::default(), 64), (Options::thorough(), 256)] {
        let a = prepare(&source, "relevant query", &options, &mut || {
            Control::Continue
        });
        let b = prepare(&source, "relevant query", &options, &mut || {
            Control::Continue
        });
        assert_eq!(a.selected, b.selected);
        assert_eq!(a.candidate_count(), cap);
        assert_eq!(a.selected.iter().collect::<BTreeSet<_>>().len(), cap);
        let parents: BTreeSet<_> = a
            .selected
            .iter()
            .step_by(2)
            .take(4)
            .map(|&index| {
                std::path::Path::new(a.snapshot.files()[a.windows[index].file].path())
                    .parent()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            parents,
            ["", "a", "b/nested", "c"]
                .into_iter()
                .map(std::path::Path::new)
                .collect()
        );
        assert!(
            a.selected.iter().skip(1).step_by(2).all(|&index| {
                a.snapshot.files()[a.windows[index].file].path() == "a/file000.rs"
            })
        );
    }
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
    assert_eq!(bounded.coverage.windows_judged, 56);
    assert_eq!(bounded.coverage.windows_unjudged, 24);
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
    let delayed = AtomicBool::new(false);
    let server = Server::new(move |request, _| {
        let mut reply = Reply::scores(request, 0.9);
        if !is_route(request) && !delayed.swap(true, Ordering::SeqCst) {
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
    assert_eq!(report.budgets.attempts, 4);
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
    assert_eq!(server.disconnected.load(Ordering::SeqCst), 3);
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

#[tokio::test]
async fn result_limit_keeps_other_directories_before_repeated_high_scores() {
    let root = fixture(0);
    let mut all: Vec<_> = (0..10)
        .map(|index| (format!("a/file{index:02}.rs"), 1.0))
        .collect();
    all.extend(
        [
            ("b/first.rs", 0.98),
            ("b/second.rs", 0.97),
            ("c/source.rs", 0.96),
            ("root.rs", 0.95),
        ]
        .map(|(path, probability)| (path.to_owned(), probability)),
    );
    for (path, _) in &all {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "fn item() {}\n").unwrap();
    }
    let scores: BTreeMap<_, _> = all.iter().cloned().collect();
    let server = Server::new(move |request, _| {
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        let answers: Vec<_> = input["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|candidate| {
                json!({"type": "predicate", "name": candidate["name"], "probability": scores[candidate["path"].as_str().unwrap()]})
            })
            .collect();
        Reply {
            status: 200,
            body: json!({"answers": answers}),
            delay: Duration::ZERO,
            retry_after: None,
        }
    })
    .await;
    let diverse = [
        "a/file00.rs",
        "b/first.rs",
        "c/source.rs",
        "root.rs",
        "a/file01.rs",
        "b/second.rs",
    ]
    .map(str::to_owned)
    .to_vec();
    let original: Vec<_> = all.iter().map(|(path, _)| path.clone()).collect();
    for (limit, expected) in [(6, diverse), (14, original.clone()), (20, original)] {
        for _ in 0..2 {
            let mut report = run(
                &root,
                &server,
                Options {
                    limit,
                    ..Options::default()
                },
            )
            .await;
            assert_eq!(report.exit_code(), 0);
            assert!(report.coverage.complete);
            assert_eq!(report.omitted_results, all.len() - expected.len());
            assert_eq!(
                report
                    .results
                    .iter()
                    .map(|result| &result.path)
                    .collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
            for result in &report.results {
                let source = fs::read_to_string(root.path().join(&result.path)).unwrap();
                assert_eq!(result.excerpt, source);
                assert_eq!((result.start_byte, result.end_byte), (0, source.len()));
                assert_eq!((result.start_line, result.end_line), (1, 1));
                assert_eq!(
                    result.sha256,
                    format!("{:x}", Sha256::digest(source.as_bytes()))
                );
                assert_eq!(
                    result.probability,
                    all.iter().find(|(path, _)| path == &result.path).unwrap().1
                );
            }
            assert!(report.encode_json().unwrap().len() <= MAX_OUTPUT_BYTES);
            assert!(!report.output_truncated);
            assert_eq!(report.omitted_results, all.len() - expected.len());
        }
    }
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
    let completed = AtomicBool::new(false);
    let server = Server::new(move |request, _| {
        let mut reply = Reply::scores(request, 0.9);
        if is_route(request) || completed.swap(true, Ordering::SeqCst) {
            reply.delay = Duration::from_secs(10);
        }
        reply
    })
    .await;
    let report = run(
        &root,
        &server,
        Options {
            timeout: Duration::from_secs(2),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.operation, Operation::Incomplete);
    assert!(report.budgets.stops.contains(&"deadline"));
    assert!(report.budgets.elapsed_ms < 10_000);
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
        assert_eq!(
            result.sha256,
            format!("{:x}", Sha256::digest(source.as_bytes()))
        );
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

fn is_route(request: &Value) -> bool {
    request["questions"][0]["name"]
        .as_str()
        .unwrap()
        .starts_with('r')
}

fn request_source_ids(bodies: &[Vec<u8>]) -> Vec<usize> {
    bodies
        .iter()
        .flat_map(|body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            if is_route(&request) {
                return Vec::new();
            }
            request["questions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|question| {
                    question["name"]
                        .as_str()
                        .unwrap()
                        .strip_prefix('w')
                        .unwrap()
                        .parse()
                        .unwrap()
                })
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn delayed_routes_promote_exact_late_source_without_authorizing_results() {
    for priority in [1.0, 0.4] {
        let root = fixture(160);
        let source = Source::open(root.path()).unwrap();
        let baseline = prepare(&source, "item behavior", &Options::default(), &mut || {
            Control::Continue
        });
        assert!(!baseline.selected.contains(&159));
        let server = Server::new(move |request, _| {
            let mut reply = Reply::scores(request, 0.1);
            if is_route(request) {
                for answer in reply.body["answers"].as_array_mut().unwrap() {
                    if answer["name"] == "r159" {
                        answer["probability"] = json!(priority);
                    }
                }
                reply.delay = Duration::from_millis(200);
            }
            reply
        })
        .await;
        let provider = server.provider();
        let pending = tokio::spawn(execute(
            source,
            "item behavior".into(),
            Options::default(),
            provider,
            Arc::new(AtomicBool::new(false)),
        ));
        let started = Instant::now();
        while server.bodies().len() < 4 && started.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(request_source_ids(&server.bodies()).len(), 16);
        let report = pending.await.unwrap();
        let bodies = server.bodies();
        let ids = request_source_ids(&bodies);
        assert!(ids[16..24].contains(&159));
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
        let mut routes = 0;
        let mut route_bytes = 0;
        for body in &bodies {
            let request: Value = serde_json::from_slice(body).unwrap();
            let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
            if is_route(&request) {
                routes += 1;
                route_bytes += body.len();
                assert!(body.len() <= 48 * 1024);
                assert!(request["questions"].as_array().unwrap().len() <= 128);
            } else {
                assert!(request["questions"].as_array().unwrap().len() <= 8);
                for candidate in input["candidates"].as_array().unwrap() {
                    let index: usize = candidate["name"].as_str().unwrap()[1..].parse().unwrap();
                    assert_eq!(candidate["text"], format!("fn item_{index}() {{}}\n"));
                    assert_eq!(candidate["path"], format!("file{index:03}.rs"));
                }
            }
        }
        assert_eq!(routes, 2);
        assert!(route_bytes <= 96 * 1024);
        assert_eq!(report.budgets.attempts, bodies.len());
        assert_eq!(
            report.budgets.encoded_request_bytes,
            bodies.iter().map(Vec::len).sum::<usize>()
        );
        assert_eq!(report.coverage.windows_selected, 48);
        assert_eq!(report.coverage.windows_sent, 48);
        assert_eq!(report.coverage.windows_judged, 48);
        assert_eq!(report.coverage.windows_unjudged, 112);
        assert_eq!(report.raw_judgments.len(), 48);
        assert!(report.results.is_empty());
        assert!(!report.coverage.complete);
    }
}

#[tokio::test]
async fn failed_and_refused_routes_fall_back_without_retry_or_source_coverage() {
    for refusal in [false, true] {
        let root = fixture(80);
        let baseline = prepare(
            &Source::open(root.path()).unwrap(),
            "item behavior",
            &Options::default(),
            &mut || Control::Continue,
        );
        let server = Server::new(move |request, _| {
            if !is_route(request) { return Reply::scores(request, 0.1); }
            if !refusal { return Reply::status(503); }
            let answers: Vec<_> = request["questions"].as_array().unwrap().iter().map(|question| json!({"type": "refusal", "name": question["name"], "reason": "test"})).collect();
            Reply { status: 200, body: json!({"answers": answers}), delay: Duration::ZERO, retry_after: None }
        }).await;
        let report = run(&root, &server, Options::default()).await;
        let bodies = server.bodies();
        assert_eq!(
            bodies
                .iter()
                .filter(|body| is_route(&serde_json::from_slice(body).unwrap()))
                .count(),
            1
        );
        assert_eq!(report.budgets.retries, 0);
        assert_eq!(report.coverage.windows_refused, 0);
        assert_eq!(report.coverage.windows_judged, 56);
        assert_eq!(
            request_source_ids(&bodies)
                .into_iter()
                .collect::<BTreeSet<_>>(),
            baseline.selected[..56].iter().copied().collect()
        );
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.operation, Operation::Incomplete);
    }
}

#[tokio::test]
async fn source_retry_keeps_body_and_charge_while_routing_uses_same_ledger() {
    let failed = AtomicBool::new(false);
    let server = Server::new(move |request, _| {
        if !is_route(request) && !failed.swap(true, Ordering::SeqCst) {
            Reply::status(503)
        } else {
            Reply::scores(request, 0.1)
        }
    })
    .await;
    let report = run(&fixture(24), &server, Options::default()).await;
    let bodies = server.bodies();
    let sources: Vec<_> = bodies
        .iter()
        .filter(|body| !is_route(&serde_json::from_slice(body).unwrap()))
        .collect();
    assert_eq!(
        sources.iter().filter(|body| **body == sources[0]).count(),
        2
    );
    assert_eq!(report.budgets.attempts, 5);
    assert_eq!(report.budgets.retries, 1);
    assert_eq!(
        report.budgets.encoded_request_bytes,
        bodies.iter().map(Vec::len).sum::<usize>()
    );
    assert_eq!(report.coverage.windows_judged, 24);
    assert_eq!(report.coverage.windows_sent, 24);
    assert!(report.coverage.complete);
}

#[test]
fn route_cards_bound_escaping_long_paths_queries_and_planning_cancellation() {
    let root = fixture(0);
    let directory = root.path().join("é\\\"".repeat(30));
    fs::create_dir(&directory).unwrap();
    let text = format!("{}\n", "🦀\\\"".repeat(2000));
    fs::write(directory.join("source.rs"), &text).unwrap();
    let query = "\\\"".repeat(2048);
    let prepared = prepare(
        &Source::open(root.path()).unwrap(),
        &query,
        &Options::default(),
        &mut || Control::Continue,
    );
    let mut frontier = navigation::Frontier::new(&prepared, true);
    let routes = frontier
        .routes(&prepared, &query, &mut || Control::Continue)
        .unwrap();
    assert!(!routes.is_empty());
    assert!(routes.len() <= 2);
    for (batch, _) in routes {
        assert!(batch.encoded_len() <= 48 * 1024);
    }
    let mut ids = Vec::new();
    while let Some(next) = frontier.next(&mut || Control::Continue) {
        ids.extend(next);
    }
    assert_eq!(
        ids.into_iter().collect::<BTreeSet<_>>(),
        (0..prepared.window_count()).collect()
    );
    let mut frontier = navigation::Frontier::new(&prepared, true);
    let mut checks = 0;
    assert!(
        frontier
            .routes(&prepared, &query, &mut || {
                checks += 1;
                if checks > 2 {
                    Control::Cancel
                } else {
                    Control::Continue
                }
            })
            .unwrap()
            .is_empty()
    );
    assert!(checks > 2);
    assert!(frontier.next(&mut || Control::Cancel).is_none());
}

#[tokio::test]
async fn no_fit_route_and_source_jobs_allow_a_later_smaller_source_job() {
    let root = fixture(16);
    for index in 0..8 {
        fs::write(
            root.path().join(format!("file{index:03}.rs")),
            "\\\"".repeat(1500),
        )
        .unwrap();
    }
    let source = Source::open(root.path()).unwrap();
    let prepared = prepare(&source, "query", &Options::default(), &mut || {
        Control::Continue
    });
    let allowance = source_batch(&prepared, "query", &prepared.selected[8..16])
        .unwrap()
        .encoded_len();
    let mut frontier = navigation::Frontier::new(&prepared, true);
    let routes = frontier
        .routes(&prepared, "query", &mut || Control::Continue)
        .unwrap();
    assert!(
        routes
            .iter()
            .all(|(batch, _)| batch.encoded_len() > allowance)
    );
    let server = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let report = execute_with_policy(
        source,
        "query".into(),
        Options::default(),
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        Policy {
            max_attempts: 8,
            max_bytes: allowance,
            attempt_timeout: Duration::from_secs(1),
        },
    )
    .await;
    assert_eq!(report.budgets.attempts, 1);
    assert_eq!(report.coverage.windows_sent, 8);
    assert_eq!(report.budgets.encoded_request_bytes, allowance);
    assert_eq!(
        request_source_ids(&server.bodies()),
        prepared.selected[8..16]
    );
    assert!(!report.coverage.complete);
}

#[tokio::test]
async fn highest_priority_region_finishes_its_later_window_before_lower_regions() {
    let root = fixture(160);
    let mut text = "opaque implementation line\n".repeat(119);
    text.push_str("unique_tail_action();\n");
    fs::write(root.path().join("zz_region.rs"), &text).unwrap();
    let prepared = prepare(
        &Source::open(root.path()).unwrap(),
        "item behavior",
        &Options::default(),
        &mut || Control::Continue,
    );
    assert!(!prepared.selected.contains(&160));
    assert_eq!(prepared.windows.len(), 162);
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.1);
        if is_route(request) {
            let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
            let target = input["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["path"] == "zz_region.rs");
            if let Some(target) = target {
                for answer in reply.body["answers"].as_array_mut().unwrap() {
                    if answer["name"] == target["name"] {
                        answer["probability"] = json!(0.4);
                    }
                }
            }
        }
        reply
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    let bodies = server.bodies();
    let ids = request_source_ids(&bodies);
    assert!(ids[16..24].contains(&160));
    assert!(ids[16..24].contains(&161));
    let tail = report
        .raw_judgments
        .iter()
        .find(|judgment| judgment.name == "w161")
        .unwrap();
    assert!(text[tail.start_byte..tail.end_byte].contains("unique_tail_action();"));
    assert!(!text[..prepared.windows[160].end].contains("unique_tail_action();"));
}

fn body_map(bodies: Vec<Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    bodies
        .into_iter()
        .map(|body| {
            let value: Value = serde_json::from_slice(&body).unwrap();
            (
                value["questions"][0]["name"].as_str().unwrap().to_owned(),
                body,
            )
        })
        .collect()
}

fn assert_matching_bodies(actual: Vec<Vec<u8>>, expected: Vec<Vec<u8>>) {
    let actual = body_map(actual);
    assert_eq!(actual, body_map(expected));
    let receipts: Vec<_> = actual.iter().map(|(name, body)| json!({
        "first_candidate": name, "bytes": body.len(), "sha256": format!("{:x}", Sha256::digest(body))
    })).collect();
    eprintln!(
        "captured_request_equivalence {}",
        json!({"equal": true, "requests": receipts})
    );
}

#[tokio::test]
async fn thorough_bodies_and_ledger_match_original_static_batches() {
    for files in [16, 80, 320] {
        let root = fixture(files);
        let source = Source::open(root.path()).unwrap();
        let options = Options::thorough();
        let prepared = prepare(&source, "item behavior", &options, &mut || {
            Control::Continue
        });
        let baseline = Server::new(|request, _| Reply::scores(request, 0.1)).await;
        let mut expected = Reservation::default();
        for indices in prepared.selected.chunks(BATCH_SIZE) {
            let batch = source_batch(&prepared, "item behavior", indices).unwrap();
            expected = reserve(expected, batch.encoded_len(), 32, 1024 * 1024).unwrap();
            baseline.provider().attempt(&batch).await.unwrap();
        }
        let current = Server::new(|request, _| Reply::scores(request, 0.1)).await;
        let report = run(&root, &current, options).await;
        assert!(
            current
                .bodies()
                .iter()
                .all(|body| !is_route(&serde_json::from_slice(body).unwrap()))
        );
        assert_matching_bodies(current.bodies(), baseline.bodies());
        assert_eq!(report.budgets.attempts, expected.attempts);
        assert_eq!(report.budgets.encoded_request_bytes, expected.bytes);
        assert_eq!(report.coverage.windows_selected, prepared.selected.len());
        assert_eq!(report.coverage.windows_judged, prepared.selected.len());
    }
}

#[tokio::test]
async fn thorough_no_fit_never_admits_windows_outside_original_selection() {
    let root = fixture(300);
    for index in 0..256 {
        fs::write(
            root.path().join(format!("file{index:03}.rs")),
            "\\\"".repeat(1500),
        )
        .unwrap();
    }
    let source = Source::open(root.path()).unwrap();
    let options = Options::thorough();
    let prepared = prepare(&source, "neutral", &options, &mut || Control::Continue);
    assert_eq!(prepared.selected, (0..256).collect::<Vec<_>>());
    let policy = Policy {
        max_attempts: 32,
        max_bytes: 6000,
        attempt_timeout: Duration::from_secs(1),
    };
    for indices in prepared.selected.chunks(BATCH_SIZE) {
        let batch = source_batch(&prepared, "neutral", indices).unwrap();
        assert!(
            reserve(
                Reservation::default(),
                batch.encoded_len(),
                policy.max_attempts,
                policy.max_bytes
            )
            .is_none()
        );
    }
    let later = source_batch(&prepared, "neutral", &(256..264).collect::<Vec<_>>()).unwrap();
    assert!(later.encoded_len() <= policy.max_bytes);
    let server = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let report = execute_with_policy(
        source,
        "neutral".into(),
        options,
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        policy,
    )
    .await;
    assert!(server.bodies().is_empty());
    assert_eq!(report.budgets.attempts, 0);
    assert_eq!(report.budgets.encoded_request_bytes, 0);
    assert_eq!(report.coverage.windows_selected, 256);
    assert_eq!(report.coverage.windows_unjudged, 300);
    assert!(report.budgets.stops.contains(&"request_byte_limit"));
}

#[tokio::test]
async fn default_keeps_the_same_route_and_source_bodies() {
    let root = fixture(160);
    let source = Source::open(root.path()).unwrap();
    let prepared = prepare(&source, "item behavior", &Options::default(), &mut || {
        Control::Continue
    });
    let mut frontier = navigation::Frontier::new(&prepared, true);
    let routes = frontier
        .routes(&prepared, "item behavior", &mut || Control::Continue)
        .unwrap();
    assert_eq!(routes.len(), 2);
    let baseline = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let mut expected = Reservation::default();
    for (batch, _) in routes {
        expected = reserve(expected, batch.encoded_len(), 8, 256 * 1024).unwrap();
        baseline.provider().attempt(&batch).await.unwrap();
    }
    for indices in prepared.selected[..48].chunks(BATCH_SIZE) {
        let batch = source_batch(&prepared, "item behavior", indices).unwrap();
        expected = reserve(expected, batch.encoded_len(), 8, 256 * 1024).unwrap();
        baseline.provider().attempt(&batch).await.unwrap();
    }
    let current = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let report = run(&root, &current, Options::default()).await;
    assert_matching_bodies(current.bodies(), baseline.bodies());
    assert_eq!(report.budgets.attempts, expected.attempts);
    assert_eq!(report.budgets.encoded_request_bytes, expected.bytes);
    assert_eq!(report.coverage.windows_judged, 48);
}
