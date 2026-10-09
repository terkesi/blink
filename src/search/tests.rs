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
        let answers: Vec<_> = request["questions"].as_array().unwrap().iter().rev().map(|question| if question["type"] == "choice" {
            let probabilities: Vec<_> = question["choices"].as_array().unwrap().iter().map(|choice| json!({"value": choice["value"], "probability": if choice["value"] == "none" { 1.0 } else { 0.0 }})).collect();
            json!({"type": "choice", "name": question["name"], "choice": "none", "probabilities": probabilities, "confidence": 1.0})
        } else {
            json!({"type": "predicate", "name": question["name"], "probability": probability})
        }).collect();
        Self {
            status: 200,
            body: json!({"answers": answers}),
            delay: Duration::ZERO,
            retry_after: None,
        }
    }
    fn nominate(request: &Value, probability: f64, nominee: &str, choice: f64) -> Self {
        let mut reply = Self::scores(request, probability);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            if answer["type"] == "choice" {
                for entry in answer["probabilities"].as_array_mut().unwrap() {
                    entry["probability"] = json!(if entry["value"] == nominee {
                        choice
                    } else if entry["value"] == "none" {
                        1.0 - choice
                    } else {
                        0.0
                    });
                }
                answer["choice"] = json!(nominee);
            }
        }
        reply
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
/// The loopback tests drive the coordinator with the compact policy the suite was written
/// against (8 initial attempts, 256 KiB, 5-second attempts); production defaults are larger
/// and are pinned by `production_defaults_are_the_documented_limits`.
fn test_policy(options: &Options) -> Policy {
    if options.thorough {
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

async fn run(root: &TempDir, server: &Server, options: Options) -> Report {
    let policy = test_policy(&options);
    execute_with_policy(
        Source::open(root.path()).unwrap(),
        "item behavior".into(),
        options,
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        policy,
    )
    .await
}

#[tokio::test]
async fn production_defaults_are_the_documented_limits() {
    let server = Server::new(|request, _| Reply::scores(request, 0.9)).await;
    let report = execute(
        Source::open(fixture(4).path()).unwrap(),
        "item behavior".into(),
        Options::default(),
        server.provider(),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    assert_eq!(report.budgets.max_attempts, 48);
    assert_eq!(report.budgets.max_encoded_request_bytes, 1568 * 1024);
    assert_eq!(report.budgets.max_concurrent_requests, 16);
    assert_eq!(report.budgets.timeout_ms, 30_000);
    let report = execute(
        Source::open(fixture(4).path()).unwrap(),
        "item behavior".into(),
        Options::thorough(),
        server.provider(),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    assert_eq!(report.budgets.max_attempts, 64);
    assert_eq!(report.budgets.max_encoded_request_bytes, 2 * 1024 * 1024);
    assert_eq!(report.budgets.timeout_ms, 60_000);
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
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "line",
        test_policy(&Options::default()),
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
    let a = prepare_with_policy(
        &source,
        "relevant query",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let b = prepare_with_policy(
        &source,
        "relevant query",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
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
    let cancelled = prepare_with_policy(
        &source,
        "query",
        test_policy(&Options::default()),
        &mut || Control::Cancel,
    );
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
        let prepared = prepare_with_policy(
            &source,
            "relevant query",
            test_policy(&options),
            &mut || Control::Continue,
        );
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
        let a = prepare_with_policy(
            &source,
            "relevant query",
            test_policy(&options),
            &mut || Control::Continue,
        );
        let b = prepare_with_policy(
            &source,
            "relevant query",
            test_policy(&options),
            &mut || Control::Continue,
        );
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
    let peak = server.peak.load(Ordering::SeqCst);
    assert!((4..=CONCURRENCY).contains(&peak), "peak {peak}");
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
    let options = Options::thorough();
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
            timeout: Duration::from_secs(1),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(report.exit_code(), 3);
    assert!(report.budgets.stops.contains(&"deadline"));
    assert!(report.budgets.elapsed_ms < 3000);
    assert_eq!(server.bodies().len(), 3);
    assert_eq!(report.coverage.windows_judged, 0);
    assert!(report.results.is_empty());
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.disconnected.load(Ordering::SeqCst) != 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("all admitted requests disconnect after the deadline");
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
    assert!(report.errors.is_empty());
    assert_eq!(report.operation, Operation::Completed);
    assert_eq!(report.coverage.windows_refused, 1);
    assert_eq!(report.coverage.windows_judged, 0);
    assert_eq!(server.bodies().len(), 1);
}

#[tokio::test]
async fn refused_source_window_is_asked_alone_and_uses_retry_score() {
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.9);
        if request["questions"].as_array().unwrap().len() > 1 {
            for answer in reply.body["answers"].as_array_mut().unwrap() {
                if answer["name"] == "w1" {
                    *answer = json!({"type": "refusal", "name": "w1", "reason": "safe refusal"});
                }
            }
        }
        reply
    })
    .await;
    let report = run(&fixture(2), &server, Options::default()).await;
    assert!(report.errors.is_empty());
    assert_eq!(report.operation, Operation::Completed);
    assert_eq!(report.coverage.windows_refused, 0);
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|j| j.name == "w1")
            .unwrap()
            .probability,
        Some(0.9)
    );
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    let retry: Value = serde_json::from_slice(&bodies[1]).unwrap();
    let questions = retry["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["name"], "w1");
    assert_eq!(
        report
            .judgment_events
            .iter()
            .filter(|e| e.name == "w1")
            .map(|e| e.probability)
            .collect::<Vec<_>>(),
        [None, Some(0.9)]
    );
}

#[tokio::test]
async fn window_refused_twice_stays_unjudged_without_error() {
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.9);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            if answer["name"] == "w1" {
                *answer = json!({"type": "refusal", "name": "w1", "reason": "safe refusal"});
            }
        }
        reply
    })
    .await;
    let report = run(&fixture(2), &server, Options::default()).await;
    assert!(report.errors.is_empty());
    assert_eq!(report.operation, Operation::Completed);
    assert_eq!(report.coverage.windows_refused, 1);
    assert!(!report.coverage.complete);
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|j| j.name == "w1")
            .unwrap()
            .probability,
        None
    );
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    let retry: Value = serde_json::from_slice(&bodies[1]).unwrap();
    assert_eq!(retry["questions"].as_array().unwrap().len(), 1);
    assert_eq!(
        report
            .judgment_events
            .iter()
            .filter(|e| e.name == "w1")
            .map(|e| e.probability)
            .collect::<Vec<_>>(),
        [None, None]
    );
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
        Options::thorough(),
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
                .filter(|question| question["type"] != "choice")
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
        let baseline = prepare_with_policy(
            &source,
            "item behavior",
            test_policy(&Options::default()),
            &mut || Control::Continue,
        );
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
        let pending = tokio::spawn(execute_with_policy(
            source,
            "item behavior".into(),
            Options::default(),
            provider,
            Arc::new(AtomicBool::new(false)),
            test_policy(&Options::default()),
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
                assert!(question_names(&request).len() <= 8);
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
async fn terminal_and_refused_routes_fall_back_without_retry_or_source_coverage() {
    for refusal in [false, true] {
        let root = fixture(80);
        let baseline = prepare_with_policy(
            &Source::open(root.path()).unwrap(),
            "item behavior",
            test_policy(&Options::default()),
            &mut || Control::Continue,
        );
        let server = Server::new(move |request, _| {
            if !is_route(request) { return Reply::scores(request, 0.1); }
            if !refusal { return Reply::status(400); }
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
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        &query,
        test_policy(&Options::default()),
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
    let prepared = prepare_with_policy(
        &source,
        "query",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
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
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "item behavior",
        test_policy(&Options::default()),
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
        let prepared =
            prepare_with_policy(&source, "item behavior", test_policy(&options), &mut || {
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
    let prepared = prepare_with_policy(&source, "neutral", test_policy(&options), &mut || {
        Control::Continue
    });
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
    let prepared = prepare_with_policy(
        &source,
        "item behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
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

fn related_fixture() -> TempDir {
    let root = fixture(0);
    fs::write(
        root.path().join("a.rs"),
        "fn caller() { distinctive_helper(); }\n",
    )
    .unwrap();
    fs::write(root.path().join("b.rs"), "fn distinctive_helper() {}\n").unwrap();
    root
}

fn related_fixture_two_targets() -> TempDir {
    let root = related_fixture();
    fs::write(root.path().join("c.rs"), "distinctive_helper other_name\n").unwrap();
    root
}
fn evidence_donor(request: &Value) -> bool {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    input["related_source"]["name"].as_str() == Some("evidence")
}
fn has_context(request: &Value) -> bool {
    request["questions"][0]["instructions"]
        .as_str()
        .unwrap()
        .contains("Use related_source")
}
fn initial_related_scores(request: &Value, target: f64) -> Reply {
    let mut reply = Reply::scores(request, target);
    for answer in reply.body["answers"].as_array_mut().unwrap() {
        if answer["name"] == "w0" {
            answer["probability"] = json!(0.9);
        }
    }
    reply
}

#[tokio::test]
async fn related_replaces_probabilities_in_both_directions_without_extra_coverage() {
    for (initial, replacement) in [(0.1, 0.9), (0.9, 0.1)] {
        let server = Server::new(move |request, _| {
            if has_context(request) {
                Reply::scores(request, replacement)
            } else {
                initial_related_scores(request, initial)
            }
        })
        .await;
        let report = run(&related_fixture(), &server, Options::default()).await;
        assert_eq!(
            report
                .raw_judgments
                .iter()
                .find(|j| j.name == "w1")
                .unwrap()
                .probability,
            Some(replacement)
        );
        assert_eq!(
            report.results.iter().any(|r| r.path == "b.rs"),
            replacement >= 0.5
        );
        assert_eq!(report.coverage.windows_selected, 2);
        assert_eq!(report.coverage.windows_sent, 2);
        assert_eq!(report.coverage.windows_judged, 2);
        let bodies = server.bodies();
        assert_eq!(report.budgets.attempts, bodies.len());
        assert_eq!(
            report.budgets.encoded_request_bytes,
            bodies.iter().map(Vec::len).sum::<usize>()
        );
        assert_eq!(report.budgets.max_attempts, 32);
        assert_eq!(report.budgets.max_encoded_request_bytes, 1056 * 1024);
        let events: Vec<_> = report
            .judgment_events
            .iter()
            .filter(|event| event.name == "w1")
            .map(|event| (event.donor.as_deref(), event.probability))
            .collect();
        assert_eq!(
            events,
            [(None, Some(initial)), (Some("w0"), Some(replacement))]
        );
    }
}

#[tokio::test]
async fn related_refusal_and_error_retain_prior_probability_and_expose_failure() {
    for refusal in [true, false] {
        let server = Server::new(move |request, _| {
            if !has_context(request) {
                return initial_related_scores(request, 0.9);
            }
            if !refusal {
                return Reply::status(400);
            }
            let mut reply = Reply::scores(request, 0.1);
            for answer in reply.body["answers"].as_array_mut().unwrap() {
                answer.as_object_mut().unwrap().remove("probability");
                answer["type"] = json!("refusal");
                answer["refusal"] = json!("cannot judge");
            }
            reply
        })
        .await;
        let report = run(&related_fixture_two_targets(), &server, Options::default()).await;
        assert_eq!(
            report
                .raw_judgments
                .iter()
                .find(|j| j.name == "w1")
                .unwrap()
                .probability,
            Some(0.9)
        );
        if refusal {
            assert!(report.errors.is_empty());
            assert_eq!(report.operation, Operation::Completed);
            let single_context_retries = server
                .bodies()
                .iter()
                .filter(|body| {
                    let request: Value = serde_json::from_slice(body).unwrap();
                    has_context(&request)
                        && request["questions"].as_array().unwrap().len() == 1
                        && request["questions"][0]["name"] == "w1"
                })
                .count();
            assert!(single_context_retries >= 1);
            assert!(
                report
                    .judgment_events
                    .iter()
                    .filter(|e| e.name == "w1" && e.probability.is_none())
                    .count()
                    >= 2
            );
            assert!(
                report
                    .judgment_events
                    .iter()
                    .any(|e| e.name == "w1" && e.donor.as_deref() == Some("w0"))
            );
        } else {
            assert!(report.errors.iter().any(|error| error.code == "http"));
            assert_eq!(report.operation, Operation::Incomplete);
        }
    }
}

#[tokio::test]
async fn related_rechecks_donor_before_retry_and_target_before_output() {
    for donor_change in [true, false] {
        let root = related_fixture();
        let path = root.path().join(if donor_change { "a.rs" } else { "b.rs" });
        let server = Server::new(move |request, _| {
            if !has_context(request) {
                return initial_related_scores(request, 0.1);
            }
            fs::write(&path, "changed source\n").unwrap();
            if donor_change {
                Reply::status(503)
            } else {
                Reply::scores(request, 0.9)
            }
        })
        .await;
        let report = run(&root, &server, Options::default()).await;
        assert_eq!(server.bodies().len(), 2);
        assert!(report.changed_files.contains(&if donor_change {
            "a.rs".into()
        } else {
            "b.rs".into()
        }));
        assert!(!report.results.iter().any(|r| r.path == "b.rs"));
        if donor_change {
            assert_eq!(
                report
                    .raw_judgments
                    .iter()
                    .find(|j| j.name == "w1")
                    .unwrap()
                    .probability,
                Some(0.1)
            );
            assert!(report.errors.iter().any(|e| e.code == "transient_http"));
        }
    }
}

#[tokio::test]
async fn related_admits_original_unjudged_windows_with_unique_coverage() {
    let root = fixture(0);
    for index in 0..80 {
        fs::write(
            root.path().join(format!("f{index:03}.rs")),
            format!("fn distinctive_helper() {{ step_{index}(); }}\n"),
        )
        .unwrap();
    }
    let server = Server::new(move |request, _| Reply::scores(request, 0.9)).await;
    let report = run(&root, &server, Options::default()).await;
    let bodies = server.bodies();
    let initial_names: BTreeSet<_> = bodies
        .iter()
        .filter_map(|body| {
            let value: Value = serde_json::from_slice(body).unwrap();
            if has_context(&value) {
                None
            } else {
                Some(
                    value["questions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|q| {
                            q["name"]
                                .as_str()
                                .filter(|n| n.starts_with('w'))
                                .map(str::to_owned)
                        })
                        .collect::<Vec<_>>(),
                )
            }
        })
        .flatten()
        .collect();
    assert!(
        report
            .raw_judgments
            .iter()
            .any(|j| !initial_names.contains(&j.name) && j.probability == Some(0.9))
    );
    assert!(report.coverage.windows_judged > initial_names.len());
    assert_eq!(report.coverage.windows_sent, report.sent_names.len());
    assert_eq!(report.budgets.attempts, bodies.len());
    assert_eq!(
        report.budgets.encoded_request_bytes,
        bodies.iter().map(Vec::len).sum::<usize>()
    );
}

#[tokio::test]
async fn related_retry_body_is_immutable_and_recovery_clears_failure() {
    let failed = Arc::new(AtomicBool::new(false));
    let server = Server::new(move |request, _| {
        if !has_context(request) {
            return initial_related_scores(request, 0.1);
        }
        if !failed.swap(true, Ordering::SeqCst) {
            Reply::status(503)
        } else {
            Reply::scores(request, 0.9)
        }
    })
    .await;
    let report = run(&related_fixture(), &server, Options::default()).await;
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[1], bodies[2]);
    assert_eq!(report.budgets.retries, 1);
    assert_eq!(
        report.budgets.encoded_request_bytes,
        bodies.iter().map(Vec::len).sum::<usize>()
    );
    assert!(report.errors.is_empty());
    assert!(report.results.iter().any(|r| r.path == "b.rs"));
}

#[tokio::test]
async fn related_deadline_and_cancellation_restore_initial_verified_results() {
    for cancel in [false, true] {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let server = Server::new(move |request, _| {
            if !has_context(request) {
                return initial_related_scores(request, 0.1);
            }
            if cancel {
                signal.store(true, Ordering::SeqCst);
            }
            let mut reply = Reply::scores(request, 0.9);
            reply.delay = Duration::from_secs(1);
            reply
        })
        .await;
        let root = related_fixture();
        let report = execute(
            Source::open(root.path()).unwrap(),
            "helper".into(),
            Options {
                timeout: Duration::from_millis(150),
                ..Options::default()
            },
            server.provider(),
            cancelled,
        )
        .await;
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].path, "a.rs");
        assert_eq!(
            report
                .raw_judgments
                .iter()
                .find(|j| j.name == "w1")
                .unwrap()
                .probability,
            Some(0.1)
        );
        assert!(
            report
                .budgets
                .stops
                .contains(&if cancel { "cancelled" } else { "deadline" })
        );
    }
}

#[test]
fn related_plan_packs_discontiguous_admissions_before_rejudgments() {
    let root = fixture(0);
    for index in 0..30 {
        fs::write(
            root.path().join(format!("f{index:03}.rs")),
            if index % 2 == 0 {
                "link_alpha"
            } else {
                "link_bravo"
            },
        )
        .unwrap();
    }
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let initial = BTreeMap::from([
        (0, Some(0.9)),
        (1, Some(0.9)),
        (2, Some(0.1)),
        (3, Some(0.1)),
    ]);
    let (jobs, stop) = related::plan(
        &prepared,
        "behavior",
        &initial,
        &BTreeSet::from([0, 1]),
        0.5,
        Reservation {
            attempts: 8,
            bytes: 1000,
        },
        Policy {
            max_attempts: 16,
            max_bytes: 512 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    assert_eq!(stop, None);
    let plan: Vec<_> = jobs
        .iter()
        .map(|job| match &job.purpose {
            Purpose::Related { targets, donor } => (*donor, targets.clone()),
            _ => panic!("related jobs only"),
        })
        .collect();
    assert_eq!(plan[0], (0, vec![4, 6, 8, 10, 12, 14, 16, 18]));
    assert_eq!(plan[1], (1, vec![5, 7, 9, 11, 13, 15, 17, 19]));
    assert_eq!(plan[2], (0, vec![20, 22, 24, 26, 28, 2]));
    assert_eq!(plan[3], (1, vec![21, 23, 25, 27, 29, 3]));
    let actual_bytes: usize = jobs.iter().map(|job| job.batch.encoded_len()).sum();
    eprintln!(
        "related_plan_receipt {}",
        json!({"jobs":plan, "actual_bytes":actual_bytes, "evidence":INCLUDE_RELATED_EVIDENCE})
    );
}

#[tokio::test]
async fn related_mutated_donor_is_abandoned_before_later_dispatch() {
    let root = fixture(0);
    for index in 0..80 {
        fs::write(
            root.path().join(format!("f{index:03}.rs")),
            format!("fn distinctive_helper() {{ step_{index}(); }}\n"),
        )
        .unwrap();
    }
    let donor = root.path().join("f000.rs");
    let server = Server::new(move |request, _| {
        if has_context(request) {
            fs::write(&donor, "changed\n").unwrap();
            Reply::scores(request, 0.1)
        } else {
            initial_related_scores(request, 0.1)
        }
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    let related_count = server
        .bodies()
        .iter()
        .filter(|b| has_context(&serde_json::from_slice(b).unwrap()))
        .count();
    assert!(related_count > 0 && related_count <= CONCURRENCY);
    assert!(report.changed_files.contains(&"f000.rs".to_owned()));
    assert!(!report.results.iter().any(|r| r.path == "f000.rs"));
}

#[tokio::test]
async fn related_success_cannot_erase_initial_refusal_or_grow_donors() {
    let root = related_fixture();
    fs::write(
        root.path().join("b.rs"),
        "distinctive_helper secondary_relation\n",
    )
    .unwrap();
    fs::write(root.path().join("c.rs"), "secondary_relation\n").unwrap();
    let server = Server::new(|request, _| {
        if has_context(request) {
            return Reply::scores(request, 0.9);
        }
        let mut reply = initial_related_scores(request, 0.1);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            if answer["name"] == "w1" {
                answer["type"] = json!("refusal");
                answer.as_object_mut().unwrap().remove("probability");
            }
        }
        reply
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|j| j.name == "w1")
            .unwrap()
            .probability,
        Some(0.9)
    );
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|j| j.name == "w2")
            .unwrap()
            .probability,
        Some(0.9)
    );
    assert!(
        report
            .judgment_events
            .iter()
            .any(|e| e.name == "w2" && e.donor.as_deref() == Some("evidence"))
    );
    assert!(report.errors.is_empty());
    assert_eq!(report.operation, Operation::Completed);
    assert_eq!(server.bodies().len(), 4);
}

#[test]
fn related_plan_prioritizes_stronger_relationship_before_source_ordinal() {
    let root = fixture(0);
    for index in 0..20 {
        let text = if index == 0 {
            "shared_name much_longer_shared_identifier"
        } else if index >= 16 {
            "much_longer_shared_identifier"
        } else {
            "shared_name"
        };
        fs::write(root.path().join(format!("f{index:03}.rs")), text).unwrap();
    }
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let (jobs, _) = related::plan(
        &prepared,
        "behavior",
        &BTreeMap::from([(0, Some(0.9)), (19, Some(0.1))]),
        &BTreeSet::from([0]),
        0.5,
        Reservation {
            attempts: 8,
            bytes: 1000,
        },
        Policy {
            max_attempts: 16,
            max_bytes: 512 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    let plan: Vec<_> = jobs
        .iter()
        .map(|job| match &job.purpose {
            Purpose::Related { targets, donor } => (*donor, targets.clone()),
            _ => panic!("related jobs only"),
        })
        .collect();
    assert_eq!(plan[0], (0, vec![16, 17, 18, 19, 1, 2, 3, 4]));
    assert_eq!(plan[1], (0, vec![5, 6, 7, 8, 9, 10, 11, 12]));
    assert_eq!(plan[2], (0, vec![13, 14, 15]));
    eprintln!(
        "related_priority_receipt {}",
        json!({"jobs":plan, "actual_bytes":jobs.iter().map(|job| job.batch.encoded_len()).sum::<usize>(), "evidence":INCLUDE_RELATED_EVIDENCE})
    );
}

#[tokio::test]
async fn stopped_completed_outcomes_preserve_errors_without_accepting_judgments() {
    let server = Server::new(|_, _| Reply::status(503)).await;
    let batch = Batch::encode(
        "behavior",
        &[Candidate {
            name: "w0",
            path: "a.rs",
            text: "source",
            start_line: 1,
            end_line: 1,
        }],
    )
    .unwrap();
    let failed = Attempt::Response(server.provider().attempt(&batch).await);
    let error = failed
        .stopped_error()
        .expect("observed HTTP error survives stop");
    assert_eq!(error.code, "transient_http");
    assert_eq!(error.status, Some(503));
    let refused = Attempt::Response(Ok(vec![Judgment {
        name: "w0".into(),
        probability: None,
        choice: None,
    }]));
    assert_eq!(refused.stopped_error().unwrap().code, "provider_refusal");
    assert_eq!(
        Attempt::Deadline.stopped_error().unwrap().code,
        "attempt_timeout"
    );
    let accepted = Attempt::Response(Ok(vec![Judgment {
        name: "w0".into(),
        probability: Some(0.9),
        choice: None,
    }]));
    assert!(accepted.stopped_error().is_none());
}

fn retry_headroom_fixture() -> TempDir {
    let root = fixture(0);
    for index in 0..160 {
        fs::write(
            root.path().join(format!("f{index:03}.rs")),
            format!("fn distinctive_helper() {{ step_{index}(); }}\n"),
        )
        .unwrap();
    }
    root
}

#[tokio::test]
async fn retry_headroom_success_body_receipt() {
    let server = Server::new(|request, _| Reply::scores(request, 0.9)).await;
    let report = run(&retry_headroom_fixture(), &server, Options::default()).await;
    let mut bodies = server.bodies();
    assert_eq!(bodies.len(), 16);
    bodies.sort();
    println!(
        "success_body_receipt {}",
        json!({"bodies": bodies.iter().map(|b| format!("{:x}", Sha256::digest(b))).collect::<Vec<_>>(), "sent": report.sent_names})
    );
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn late_related_retries_use_unspent_follow_up_and_headroom_with_exact_bodies() {
    for failures in [1, 8] {
        let server = Server::new(move |request, number| {
            if (17 - failures..=16).contains(&number) {
                Reply::status(503)
            } else {
                Reply::scores(request, 0.9)
            }
        })
        .await;
        let report = run(&retry_headroom_fixture(), &server, Options::default()).await;
        let bodies = server.bodies();
        assert_eq!(bodies.len(), 16 + failures.min(10));
        assert_eq!(report.budgets.retries, failures.min(10));
        assert!(report.budgets.encoded_request_bytes <= 896 * 1024);
        assert_eq!(
            report.budgets.encoded_request_bytes,
            bodies.iter().map(Vec::len).sum::<usize>()
        );
        for retried in &bodies[16..] {
            assert!(bodies[..16].contains(retried));
        }
        assert!(
            report.errors.is_empty(),
            "every failed request was retried within the headroom"
        );
    }
}

#[tokio::test]
async fn route_retry_holds_barrier_and_recovers_once() {
    let routes = AtomicUsize::new(0);
    let sources = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&sources);
    let server = Server::new(move |request, _| {
        if is_route(request) {
            if routes.fetch_add(1, Ordering::SeqCst) == 0 {
                return Reply::status(503);
            }
            assert_eq!(observed.load(Ordering::SeqCst), 2);
        } else {
            observed.fetch_add(1, Ordering::SeqCst);
        }
        Reply::scores(request, 0.1)
    })
    .await;
    let report = run(&fixture(80), &server, Options::default()).await;
    let routes: Vec<_> = server
        .bodies()
        .into_iter()
        .filter(|body| is_route(&serde_json::from_slice(body).unwrap()))
        .collect();
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0], routes[1]);
    assert_eq!(report.budgets.retries, 1);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn in_flight_retry_keeps_observed_error_at_deadline() {
    let server = Server::new(|request, number| {
        if number == 1 {
            Reply::status(503)
        } else {
            let mut reply = Reply::scores(request, 0.9);
            reply.delay = Duration::from_secs(2);
            reply
        }
    })
    .await;
    let report = run(
        &fixture(1),
        &server,
        Options {
            timeout: Duration::from_millis(350),
            ..Options::default()
        },
    )
    .await;
    assert_eq!(report.budgets.retries, 1);
    assert!(report.budgets.stops.contains(&"deadline"));
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "transient_http" && e.status == Some(503))
    );
}

#[test]
fn declarations_and_calls_follow_common_language_forms() {
    let declared = callees::declarations(
        "class Timeout:\n    def as_dict(self) -> dict:\nasync def fetch(url):\n\
         pub(crate) fn merge(a: u8)\npub const fn new() -> Self\nimpl Window {\nstruct Window {\n\
         func (c *Client) Do(req *Request)\nfunc Parse(s string)\n\
         export function stripAuth(url: string)\nexport const parse = (input) =>\n  private async load(id) {\n\
         return helper(value)\ndefault:\ntype(x)\nlet Some(x) = y else { return };\nlet Point { x, y } = p;\n\
         let len = 3;\nconst handler = async (req) => {\nlet add = |a, b| a + b;\n\
         export default function(req) {\nlet f = asyncThing();\nlet mut handle = |x| x;\n",
    );
    assert_eq!(
        declared,
        BTreeSet::from([
            "Do",
            "Parse",
            "Timeout",
            "Window",
            "add",
            "as_dict",
            "fetch",
            "handle",
            "handler",
            "load",
            "merge",
            "new",
            "parse",
            "stripAuth"
        ])
    );
    assert_eq!(
        callees::calls("x = Timeout(timeout).as_dict()\nif (ready) { run (1); 2(y) }"),
        BTreeSet::from(["Timeout", "as_dict", "if", "run"])
    );
    assert!(
        callees::calls("def update(self):\nclass Cookies(Base):\npub fn new(a: u8)\n").is_empty()
    );
}

#[test]
fn callee_plan_adds_unplanned_unaccepted_definitions_with_their_best_caller() {
    let root = fixture(0);
    let files = [
        (
            "f000.rs",
            "run_task(job);\nshared_step(x);\naccepted_helper();\n",
        ),
        ("f001.rs", "run_task(other);\n"),
        ("f002.py", "def run_task(job):\n    pass\n"),
        ("f003.py", "def shared_step(x):\n    pass\n"),
        ("f004.py", "def shared_step(y):\n    pass\n"),
        ("f005.py", "def accepted_helper():\n    pass\n"),
        ("f006.rs", "only_c_calls(z);\n"),
        ("f007.py", "def only_c_calls(z):\n    pass\n"),
        ("f008.rs", "only_d_calls(z);\n"),
        ("f009.py", "def only_d_calls(z):\n    pass\n"),
    ];
    for (name, text) in files {
        fs::write(root.path().join(name), text).unwrap();
    }
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let initial = BTreeMap::from([
        (0, Some(0.95)),
        (1, Some(0.7)),
        (2, Some(0.1)),
        (5, Some(0.8)),
        (6, Some(0.6)),
        (8, Some(0.55)),
    ]);
    let fresh = BTreeSet::from([0, 1, 5, 6, 8]);
    let plan = |planned: &VecDeque<Job>| -> Vec<(usize, Vec<usize>)> {
        callees::plan(
            &prepared,
            "behavior",
            &initial,
            &fresh,
            0.5,
            planned,
            Reservation {
                attempts: 8,
                bytes: 1000,
            },
            Policy {
                max_attempts: 18,
                max_bytes: 608 * 1024,
                attempt_timeout: Duration::from_secs(5),
            },
            &mut || Control::Continue,
        )
        .iter()
        .map(|job| match &job.purpose {
            Purpose::Related { targets, donor } => (*donor, targets.clone()),
            _ => panic!("related jobs only"),
        })
        .collect()
    };
    assert_eq!(plan(&VecDeque::new()), [(0, vec![2, 3, 4]), (6, vec![7])]);
    let name = "w2".to_owned();
    let planned = VecDeque::from([Job {
        batch: Batch::encode("behavior", &[candidate(&prepared, 2, &name)]).unwrap(),
        purpose: Purpose::Related {
            targets: vec![2],
            donor: 0,
        },
        pending_retry: None,
        refusal_retry: false,
        ready: tokio::time::Instant::now(),
    }]);
    assert_eq!(plan(&planned), [(6, vec![7]), (8, vec![9])]);
}

#[tokio::test]
async fn callee_jobs_follow_the_unchanged_shared_word_jobs() {
    let root = fixture(0);
    fs::write(
        root.path().join("f000.rs"),
        "much_longer_shared_identifier\nhlp();\n",
    )
    .unwrap();
    for index in 1..100 {
        fs::write(
            root.path().join(format!("f{index:03}.rs")),
            "much_longer_shared_identifier\n",
        )
        .unwrap();
    }
    fs::write(root.path().join("f100.py"), "def hlp():\n    pass\n").unwrap();
    let server = Server::new(|request, _| {
        if has_context(request) {
            Reply::scores(request, 0.1)
        } else {
            initial_related_scores(request, 0.1)
        }
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    let related: Vec<Value> = server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .filter(|request| has_context(request) && !evidence_donor(request))
        .collect();
    assert_eq!(related.len(), 9);
    let names = |request: &Value| -> Vec<String> {
        request["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|question| question["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(
        related[..8]
            .iter()
            .all(|request| !names(request).contains(&"w100".to_owned()))
    );
    assert_eq!(names(&related[8]), ["w100"]);
    assert_eq!(report.budgets.attempts, 21);
    assert_eq!(
        report
            .judgment_events
            .iter()
            .filter(|event| event.name == "w100" && event.donor.is_some())
            .map(|event| event.donor.as_deref())
            .collect::<Vec<_>>(),
        [Some("w0")]
    );
}

fn callee_plan_for(root: &TempDir, initial: &[(usize, f64)]) -> Vec<(usize, Vec<usize>)> {
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let initial: BTreeMap<_, _> = initial.iter().map(|&(index, p)| (index, Some(p))).collect();
    let fresh = initial
        .keys()
        .map(|&index| prepared.windows[index].file)
        .collect();
    callees::plan(
        &prepared,
        "behavior",
        &initial,
        &fresh,
        0.5,
        &VecDeque::new(),
        Reservation {
            attempts: 8,
            bytes: 1000,
        },
        Policy {
            max_attempts: 18,
            max_bytes: 608 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    )
    .iter()
    .map(|job| match &job.purpose {
        Purpose::Related { targets, donor } => (*donor, targets.clone()),
        _ => panic!("related jobs only"),
    })
    .collect()
}

#[test]
fn callee_plan_ranks_rarest_names_includes_bodies_and_skips_oversized_donors() {
    let root = fixture(0);
    let def = |name: &str| format!("def {name}():\n    pass\n");
    let files = [
        (
            "f000.rs".to_owned(),
            "common_name(); rare_name(); mid_name(); tail_name();\n".to_owned(),
        ),
        (
            "f001.py".to_owned(),
            format!("{}{}", def("common_name"), def("rare_name")),
        ),
        ("f002.py".to_owned(), def("common_name")),
        ("f003.py".to_owned(), def("common_name")),
        ("f004.py".to_owned(), def("mid_name")),
        ("f005.py".to_owned(), def("mid_name")),
        (
            "f006.py".to_owned(),
            format!(
                "{}def tail_name():\n{}",
                "# pad\n".repeat(74),
                "    step = 1\n".repeat(78)
            ),
        ),
    ];
    for (name, text) in &files {
        fs::write(root.path().join(name), text).unwrap();
    }
    assert_eq!(
        callee_plan_for(&root, &[(0, 0.9)]),
        [(0, vec![1, 6, 7, 8, 4, 5, 2, 3])]
    );
    let oversized = fixture(0);
    fs::write(
        oversized.path().join("f000.py"),
        format!("x = Helper(1)\n# {}\n", "\"".repeat(1100)),
    )
    .unwrap();
    fs::write(oversized.path().join("f001.rs"), "Helper(2);\n").unwrap();
    fs::write(
        oversized.path().join("f002.py"),
        "class Helper:\n    pass\n",
    )
    .unwrap();
    assert_eq!(
        callee_plan_for(&oversized, &[(0, 0.95), (1, 0.6)]),
        [(1, vec![2])]
    );
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let mut checks = 0;
    let stopped = callees::plan(
        &prepared,
        "behavior",
        &BTreeMap::from([(0, Some(0.9))]),
        &BTreeSet::from([0]),
        0.5,
        &VecDeque::new(),
        Reservation {
            attempts: 8,
            bytes: 1000,
        },
        Policy {
            max_attempts: 18,
            max_bytes: 608 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || {
            checks += 1;
            if checks > prepared.windows.len() {
                Control::Deadline
            } else {
                Control::Continue
            }
        },
    );
    assert!(stopped.is_empty());
}

fn callee_fixture() -> TempDir {
    let root = fixture(0);
    fs::write(
        root.path().join("f000.rs"),
        "much_longer_shared_identifier\nhlp();\n",
    )
    .unwrap();
    for index in 1..100 {
        fs::write(
            root.path().join(format!("f{index:03}.rs")),
            "much_longer_shared_identifier\n",
        )
        .unwrap();
    }
    fs::write(root.path().join("f100.py"), "def hlp():\n    pass\n").unwrap();
    root
}

fn question_names(request: &Value) -> Vec<String> {
    request["questions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|question| question["type"] != "choice")
        .map(|question| question["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn callee_deadline_keeps_completed_shared_word_results() {
    let server = Server::new(|request, _| {
        if !has_context(request) {
            return initial_related_scores(request, 0.1);
        }
        let mut reply = Reply::scores(request, 0.9);
        if question_names(request).contains(&"w110".to_owned()) {
            reply.delay = Duration::from_secs(10);
        }
        reply
    })
    .await;
    let root = callee_fixture();
    for index in 0..10 {
        fs::write(
            root.path().join(format!("f{:03}.py", 101 + index)),
            format!("def h{index}():\n    pass\n"),
        )
        .unwrap();
    }
    let calls: String = (0..10).map(|index| format!("h{index}(); ")).collect();
    fs::write(
        root.path().join("f000.rs"),
        format!("much_longer_shared_identifier\nhlp(); {calls}\n"),
    )
    .unwrap();
    let report = execute_with_policy(
        Source::open(root.path()).unwrap(),
        "helper".into(),
        Options {
            timeout: Duration::from_secs(4),
            ..Options::default()
        },
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        Policy {
            max_attempts: 8,
            max_bytes: 256 * 1024,
            attempt_timeout: Duration::from_secs(2),
        },
    )
    .await;
    let callee = |name: &str| name[1..].parse::<usize>().unwrap() >= 100;
    let fast_callee_sent = server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body).unwrap())
        .filter(has_context)
        .map(|request| question_names(&request))
        .any(|names| !names.contains(&"w110".to_owned()) && names.iter().any(|name| callee(name)));
    assert!(fast_callee_sent);
    assert!(report.budgets.stops.contains(&"deadline"));
    assert!(
        report
            .results
            .iter()
            .any(|record| record.path.ends_with(".rs") && record.path != "f000.rs")
    );
    assert!(
        report
            .raw_judgments
            .iter()
            .filter(|judgment| callee(&judgment.name))
            .all(|judgment| judgment.probability.is_none_or(|p| p < 0.5))
    );
    assert!(
        report
            .judgment_events
            .iter()
            .any(|event| event.donor.is_some())
    );
    assert!(
        report
            .judgment_events
            .iter()
            .all(|event| !(callee(&event.name) && event.donor.is_some()))
    );
}

#[tokio::test]
async fn callee_jobs_skip_donors_retracted_by_shared_word_jobs() {
    let root = fixture(0);
    fs::write(
        root.path().join("f000.rs"),
        "much_longer_shared_identifier\nhlp();\n",
    )
    .unwrap();
    fs::write(
        root.path().join("f001.rs"),
        "much_longer_shared_identifier\n",
    )
    .unwrap();
    fs::write(root.path().join("f002.py"), "def hlp():\n    pass\n").unwrap();
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.1);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            let retract = has_context(request) && answer["name"] == "w0";
            if !retract && (answer["name"] == "w0" || answer["name"] == "w1") {
                answer["probability"] = json!(0.9);
            }
        }
        reply
    })
    .await;
    run(&root, &server, Options::default()).await;
    let related: Vec<Value> = server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .filter(|request| has_context(request) && !evidence_donor(request))
        .collect();
    assert!(!related.is_empty());
    assert!(
        related
            .iter()
            .all(|request| !question_names(request).contains(&"w2".to_owned()))
    );
}

#[tokio::test]
async fn refusal_retry_waits_for_fresh_work_and_counts_as_a_retry() {
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.9);
        if !has_context(request) && request["questions"].as_array().unwrap().len() > 1 {
            for answer in reply.body["answers"].as_array_mut().unwrap() {
                if answer["name"] == "w5" {
                    *answer = json!({"type": "refusal", "name": "w5", "reason": "safe refusal"});
                }
            }
        }
        reply
    })
    .await;
    let report = run(&retry_headroom_fixture(), &server, Options::default()).await;
    let requests: Vec<Value> = server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .collect();
    let initial: Vec<_> = requests.iter().filter(|r| !has_context(r)).collect();
    assert_eq!(initial.len(), 9);
    assert_eq!(question_names(initial[8]), ["w5"]);
    assert_eq!(
        initial
            .iter()
            .filter(|r| question_names(r).len() == 8)
            .count(),
        6
    );
    assert_eq!(requests.len() - initial.len(), 7);
    assert_eq!(report.budgets.attempts, 16);
    assert_eq!(report.budgets.retries, 1);
    assert!(report.errors.is_empty());
    assert_eq!(report.coverage.windows_refused, 0);
}

#[tokio::test]
async fn late_refusal_retry_is_dropped_instead_of_risking_the_deadline() {
    let server = Server::new(|request, _| {
        if !has_context(request) {
            return initial_related_scores(request, 0.1);
        }
        let mut reply = Reply::scores(request, 0.9);
        if request["questions"].as_array().unwrap().len() == 1 {
            reply.delay = Duration::from_secs(3);
            return reply;
        }
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            if answer["name"] == "w2" {
                *answer = json!({"type": "refusal", "name": "w2", "reason": "safe refusal"});
            }
        }
        reply
    })
    .await;
    let report = run(
        &related_fixture_two_targets(),
        &server,
        Options {
            timeout: Duration::from_secs(2),
            ..Options::default()
        },
    )
    .await;
    assert!(!report.budgets.stops.contains(&"deadline"));
    assert!(report.errors.is_empty());
    assert!(report.results.iter().any(|record| record.path == "b.rs"));
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|j| j.name == "w1")
            .unwrap()
            .probability,
        Some(0.9)
    );
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|j| j.name == "w2")
            .unwrap()
            .probability,
        Some(0.1)
    );
    assert!(server.bodies().iter().all(|body| {
        let request: Value = serde_json::from_slice(body).unwrap();
        !has_context(&request) || question_names(&request).len() == 2
    }));
}

#[tokio::test]
async fn failed_refusal_retry_is_best_effort() {
    let server = Server::new(|request, _| {
        if request["questions"].as_array().unwrap().len() == 1 {
            return Reply::status(503);
        }
        let mut reply = Reply::scores(request, 0.9);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            if answer["name"] == "w1" {
                *answer = json!({"type": "refusal", "name": "w1", "reason": "safe refusal"});
            }
        }
        reply
    })
    .await;
    let report = run(&fixture(2), &server, Options::default()).await;
    assert!(report.errors.is_empty());
    assert_eq!(report.operation, Operation::Completed);
    assert_eq!(report.coverage.windows_refused, 1);
    assert_eq!(report.budgets.retries, 1);
    assert_eq!(server.bodies().len(), 2);
}

#[tokio::test]
async fn refusal_retry_skips_a_donor_the_follow_up_rejected() {
    let server = Server::new(|request, _| {
        if !has_context(request) {
            let mut reply = Reply::scores(request, 0.1);
            for answer in reply.body["answers"].as_array_mut().unwrap() {
                if answer["name"] == "w0" || answer["name"] == "w3" {
                    answer["probability"] = json!(0.9);
                }
            }
            return reply;
        }
        let mut reply = Reply::scores(request, 0.1);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            if answer["name"] == "w2" {
                *answer = json!({"type": "refusal", "name": "w2", "reason": "safe refusal"});
            }
        }
        reply
    })
    .await;
    let root = related_fixture_two_targets();
    fs::write(root.path().join("d.rs"), "fn caller_two() { caller(); }\n").unwrap();
    let report = run(&root, &server, Options::default()).await;
    assert!(report.errors.is_empty());
    assert_eq!(
        report
            .judgment_events
            .iter()
            .filter(|e| e.name == "w2" && e.donor.is_some())
            .count(),
        1
    );
    assert!(server.bodies().iter().all(|body| {
        let request: Value = serde_json::from_slice(body).unwrap();
        question_names(&request) != ["w2"]
    }));
}

fn evidence_fixture_server(
    evidence_score: f64,
) -> impl Fn(&Value, usize) -> Reply + Send + Sync + 'static {
    move |request, _| {
        if evidence_donor(request) {
            Reply::scores(request, evidence_score)
        } else if has_context(request) {
            Reply::scores(request, 0.1)
        } else {
            initial_related_scores(request, 0.1)
        }
    }
}

#[tokio::test]
async fn evidence_pass_accepts_previously_rejected_helper_with_fresh_hash() {
    let root = related_fixture();
    let server = Server::new(evidence_fixture_server(0.9)).await;
    let report = run(&root, &server, Options::default()).await;
    let helper = report
        .results
        .iter()
        .find(|record| record.path == "b.rs")
        .expect("evidence pass accepts the rejected helper");
    let expected = format!(
        "{:x}",
        Sha256::digest(fs::read(root.path().join("b.rs")).unwrap())
    );
    assert_eq!(helper.sha256, expected);
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|judgment| judgment.name == "w1")
            .unwrap()
            .probability,
        Some(0.9)
    );
    assert!(
        report
            .judgment_events
            .iter()
            .any(|event| event.name == "w1" && event.donor.as_deref() == Some("evidence"))
    );
}

#[tokio::test]
async fn evidence_request_carries_accepted_excerpts_with_headers_within_cap() {
    let root = related_fixture();
    let server = Server::new(evidence_fixture_server(0.1)).await;
    run(&root, &server, Options::default()).await;
    let requests: Vec<Value> = server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .filter(evidence_donor)
        .collect();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request["questions"].as_array().unwrap().len() <= EVIDENCE_BATCH);
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    let related = &input["related_source"];
    assert_eq!(related["name"].as_str().unwrap(), "evidence");
    assert_eq!(related["path"].as_str().unwrap(), "a.rs");
    let text = related["text"].as_str().unwrap();
    assert!(text.len() <= EVIDENCE_BYTES);
    assert!(text.contains("// a.rs:1-1\nfn caller() { distinctive_helper(); }"));
    assert!(!text.contains("b.rs"));
}

#[tokio::test]
async fn no_evidence_phase_without_accepted_windows() {
    let root = related_fixture();
    let server = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let report = run(&root, &server, Options::default()).await;
    assert!(report.results.is_empty());
    assert!(
        server
            .bodies()
            .iter()
            .map(|body| serde_json::from_slice::<Value>(body).unwrap())
            .all(|request| !evidence_donor(&request) && !has_context(&request))
    );
}

#[tokio::test]
async fn evidence_deadline_keeps_callee_and_shared_word_results() {
    let server = Server::new(|request, _| {
        if evidence_donor(request) {
            let mut reply = Reply::scores(request, 0.9);
            reply.delay = Duration::from_secs(10);
            reply
        } else if has_context(request) {
            Reply::scores(request, 0.9)
        } else {
            initial_related_scores(request, 0.1)
        }
    })
    .await;
    let root = callee_fixture();
    let report = execute_with_policy(
        Source::open(root.path()).unwrap(),
        "helper".into(),
        Options {
            timeout: Duration::from_secs(4),
            ..Options::default()
        },
        server.provider(),
        Arc::new(AtomicBool::new(false)),
        Policy {
            max_attempts: 8,
            max_bytes: 256 * 1024,
            attempt_timeout: Duration::from_secs(2),
        },
    )
    .await;
    let evidence_sent = server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body).unwrap())
        .any(|request| evidence_donor(&request));
    assert!(evidence_sent);
    assert!(report.budgets.stops.contains(&"deadline"));
    assert_eq!(
        report
            .raw_judgments
            .iter()
            .find(|judgment| judgment.name == "w100")
            .map(|judgment| judgment.probability),
        Some(Some(0.9)),
        "callee-phase acceptance survives the evidence deadline"
    );
    assert!(
        report
            .results
            .iter()
            .any(|record| record.path.ends_with(".rs") && record.path != "f000.rs"),
        "shared-identifier acceptance survives the evidence deadline"
    );
    assert!(
        report
            .judgment_events
            .iter()
            .all(|event| event.donor.as_deref() != Some("evidence")),
        "evidence judgments roll back to the evidence-phase restore point"
    );
}

#[tokio::test]
async fn below_threshold_evidence_judgment_does_not_retract_accepted_window() {
    let root = related_fixture();
    let server = Server::new(evidence_fixture_server(0.1)).await;
    let report = run(&root, &server, Options::default()).await;
    assert!(
        report.results.iter().any(|record| record.path == "a.rs"),
        "accepted window survives a low evidence judgment"
    );
    assert!(!report.results.iter().any(|record| record.path == "b.rs"));
    assert!(
        report
            .judgment_events
            .iter()
            .any(|event| event.donor.as_deref() == Some("evidence")
                && event.probability == Some(0.1))
    );
}

#[tokio::test]
async fn thorough_mode_sends_no_evidence_request() {
    for thorough in [false, true] {
        let root = related_fixture();
        let server = Server::new(|request, _| initial_related_scores(request, 0.1)).await;
        let options = if thorough {
            Options::thorough()
        } else {
            Options::default()
        };
        run(&root, &server, options).await;
        let evidence_requests = server
            .bodies()
            .iter()
            .map(|body| serde_json::from_slice::<Value>(body).unwrap())
            .filter(evidence_donor)
            .count();
        assert_eq!(evidence_requests > 0, !thorough);
    }
}

fn evidence_card_for(
    root: &TempDir,
    accepted: &[(usize, f64)],
    rejected: &[usize],
) -> Option<evidence::Card> {
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let mut probabilities: BTreeMap<usize, Option<f64>> = accepted
        .iter()
        .map(|&(index, p)| (index, Some(p)))
        .collect();
    let mut best: BTreeMap<usize, f64> = accepted.iter().copied().collect();
    for &index in rejected {
        probabilities.insert(index, Some(0.1));
        best.insert(index, 0.1);
    }
    let fresh: BTreeSet<usize> = (0..prepared.snapshot.files().len()).collect();
    evidence::plan(
        &prepared,
        "behavior",
        &probabilities,
        &best,
        &fresh,
        &BTreeSet::new(),
        &BTreeMap::new(),
        0.5,
        Reservation {
            attempts: 18,
            bytes: 1000,
        },
        Policy {
            max_attempts: 22,
            max_bytes: 768 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    )
    .map(|(card, _)| card)
}

#[test]
fn evidence_card_merges_in_file_order_then_ranks_by_probability() {
    let root = fixture(0);
    let lines: String = (1..=200).map(|n| format!("line_{n:03}\n")).collect();
    fs::write(root.path().join("f000.py"), &lines).unwrap();
    fs::write(root.path().join("f001.py"), "other_file\n").unwrap();
    let (text, path, start, _, files) =
        evidence_card_for(&root, &[(0, 0.7), (2, 0.9)], &[1]).unwrap();
    assert_eq!((path.as_str(), start), ("f000.py", 145));
    assert!(text.starts_with("// f000.py:145-200\n"));
    assert!(text.contains("\n// f000.py:1-80\n"));
    assert_eq!(text.matches("line_050\n").count(), 1);
    assert_eq!(text.matches("line_150\n").count(), 1);
    assert_eq!(files, BTreeSet::from([0]));
    let (text, _, start, end, _) = evidence_card_for(&root, &[(0, 0.7), (1, 0.9)], &[2]).unwrap();
    assert_eq!((start, end), (1, 152));
    assert_eq!(text.matches("// f000.py:").count(), 1);
    assert_eq!(text.matches("line_076\n").count(), 1);
    assert!(evidence_card_for(&root, &[], &[1]).is_none());
}

#[test]
fn evidence_card_shrinks_until_it_encodes() {
    let root = fixture(0);
    let noisy: String = (0..60).map(|_| format!("{}\n", "\"".repeat(60))).collect();
    for index in 0..4 {
        fs::write(root.path().join(format!("f{index:03}.py")), &noisy).unwrap();
    }
    fs::write(root.path().join("f004.py"), "def target():\n    pass\n").unwrap();
    let accepted: Vec<_> = (0..4)
        .map(|index| (index, 0.9 - index as f64 * 0.01))
        .collect();
    let (text, ..) = evidence_card_for(&root, &accepted, &[4]).unwrap();
    let headers = text.matches("// f00").count();
    assert!((1..4).contains(&headers), "kept {headers} excerpts");
    let probe = Candidate {
        name: "evidence",
        path: "f000.py",
        text: &text,
        start_line: 1,
        end_line: 60,
    };
    let target = Candidate {
        name: "w4",
        path: "f004.py",
        text: "def target():\n    pass\n",
        start_line: 1,
        end_line: 2,
    };
    assert!(matches!(
        Batch::encode_with_context("behavior", &[target], &probe, EVIDENCE_LIMIT),
        Ok(Some(_))
    ));
    let single = fixture(0);
    let heavy: String = (0..71)
        .map(|_| format!("{}\n", "\\\"".repeat(28)))
        .collect();
    fs::write(single.path().join("f000.py"), &heavy).unwrap();
    fs::write(single.path().join("f001.py"), "def target():\n    pass\n").unwrap();
    let (text, ..) = evidence_card_for(&single, &[(0, 0.9)], &[1]).unwrap();
    assert!(text.len() < heavy.len());
    let probes: Vec<_> = (0..EVIDENCE_BATCH)
        .map(|n| Candidate {
            name: ["p0", "p1", "p2", "p3"][n],
            ..target
        })
        .collect();
    let card = Candidate {
        name: "evidence",
        path: "f000.py",
        text: &text,
        start_line: 1,
        end_line: 71,
    };
    assert!(matches!(
        Batch::encode_with_context("behavior", &probes, &card, EVIDENCE_LIMIT),
        Ok(Some(_))
    ));
}

#[tokio::test]
async fn evidence_request_is_skipped_when_a_card_file_changed() {
    let root = related_fixture();
    let path = root.path().join("a.rs");
    let server = Server::new(move |request, _| {
        if !has_context(request) {
            return initial_related_scores(request, 0.1);
        }
        fs::write(&path, "changed source\n").unwrap();
        Reply::scores(request, 0.1)
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    assert_eq!(report.changed_files, ["a.rs"]);
    assert!(
        server
            .bodies()
            .iter()
            .map(|body| serde_json::from_slice::<Value>(body).unwrap())
            .all(|request| !evidence_donor(&request))
    );
}

fn evidence_requests(server: &Server) -> Vec<Value> {
    server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body).unwrap())
        .filter(evidence_donor)
        .collect()
}

#[tokio::test]
async fn listwise_nomination_leads_the_evidence_pass() {
    let root = fixture(0);
    for index in 0..6 {
        fs::write(
            root.path().join(format!("f{index:03}.rs")),
            format!("fn item_{index}() {{ step_{index}(); }}\n"),
        )
        .unwrap();
    }
    let server = Server::new(|request, _| {
        if evidence_donor(request) {
            return Reply::scores(request, 0.9);
        }
        if has_context(request) {
            return Reply::scores(request, 0.1);
        }
        let mut reply = Reply::nominate(request, 0.1, "w4", 0.9);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            if answer["name"] == "w0" {
                answer["probability"] = json!(0.9);
            }
        }
        reply
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    let evidence = evidence_requests(&server);
    assert!(!evidence.is_empty());
    assert_eq!(question_names(&evidence[0])[0], "w4");
    assert!(report.results.iter().any(|record| record.path == "f004.rs"));
    assert!(
        report
            .judgment_events
            .iter()
            .any(|event| event.name == "w4" && event.donor.is_none() && event.choice == Some(0.9))
    );
}

#[tokio::test]
async fn nomination_is_checked_alone_when_nothing_was_accepted() {
    for confirmed in [true, false] {
        let root = fixture(4);
        let server = Server::new(move |request, _| {
            if question_names(request).len() == 1 {
                return Reply::scores(request, if confirmed { 0.9 } else { 0.1 });
            }
            Reply::nominate(request, 0.1, "w2", 0.8)
        })
        .await;
        let report = run(&root, &server, Options::default()).await;
        let bodies = server.bodies();
        let single: Vec<Value> = bodies
            .iter()
            .map(|body| serde_json::from_slice::<Value>(body).unwrap())
            .filter(|request| question_names(request) == ["w2"])
            .collect();
        assert_eq!(single.len(), 1);
        assert!(!has_context(&single[0]));
        assert_eq!(report.results.len(), usize::from(confirmed));
        assert!(report.errors.is_empty());
        assert_eq!(report.budgets.attempts, 2);
    }
    let server = Server::new(|request, _| Reply::scores(request, 0.1)).await;
    let report = run(&fixture(4), &server, Options::default()).await;
    assert_eq!(report.budgets.attempts, 1);
    assert!(report.results.is_empty());
}

fn deepen_fixture() -> TempDir {
    let root = fixture(300);
    let mut big = String::from("fn behavior_entry() { start(); }\n");
    for n in 1..600 {
        big.push_str(&format!("fn deep_{n:03}() {{}}\n"));
    }
    fs::write(root.path().join("big.rs"), big).unwrap();
    root
}

fn big_file_requests(server: &Server) -> Vec<Vec<usize>> {
    server
        .bodies()
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body).unwrap())
        .filter(|request| !is_route(request) && !has_context(request))
        .filter_map(|request| {
            let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
            let lines: Vec<usize> = input["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["path"] == "big.rs")
                .map(|c| c["start_line"].as_u64().unwrap() as usize)
                .collect();
            (!lines.is_empty()).then_some(lines)
        })
        .collect()
}

#[tokio::test]
async fn deepen_reads_unread_windows_of_an_accepted_file_nearest_first() {
    let server = Server::new(|request, _| {
        if is_route(request) {
            return Reply::scores(request, 0.5);
        }
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        let mut reply = Reply::scores(request, 0.1);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            let candidate = input["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == answer["name"]);
            if candidate.is_some_and(|c| c["path"] == "big.rs") {
                answer["probability"] = json!(0.9);
            }
        }
        reply
    })
    .await;
    let report = run(&deepen_fixture(), &server, Options::default()).await;
    let requests = big_file_requests(&server);
    assert_eq!(
        &requests[..2],
        [vec![1], vec![73]],
        "the initial pass reads two windows"
    );
    assert_eq!(
        requests.last().unwrap(),
        &[217, 289, 361, 433, 505, 577],
        "deepening reads what the follow-up passes left unread, nearest first"
    );
    let bodies = server.bodies();
    let position = bodies
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body).unwrap())
        .position(|request| {
            let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
            input["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == "big.rs" && c["start_line"] == 577)
        })
        .unwrap();
    assert_eq!(position, bodies.len() - 1, "deepening is the final request");
    assert!(
        report
            .results
            .iter()
            .any(|record| record.path == "big.rs" && record.end_line == 600)
    );
}

#[tokio::test]
async fn deepen_needs_an_accepted_window_and_stays_within_its_allowance() {
    let server =
        Server::new(|request, _| Reply::scores(request, if is_route(request) { 0.5 } else { 0.1 }))
            .await;
    let report = run(&deepen_fixture(), &server, Options::default()).await;
    assert_eq!(big_file_requests(&server), [vec![1], vec![73]]);
    assert!(report.results.is_empty());
    let root = fixture(300);
    let mut huge = String::from("fn behavior_entry() { start(); }\n");
    for n in 1..6000 {
        huge.push_str(&format!("fn deep_{n:04}() {{}}\n"));
    }
    fs::write(root.path().join("big.rs"), huge).unwrap();
    let server = Server::new(|request, _| {
        if is_route(request) {
            return Reply::scores(request, 0.5);
        }
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        let mut reply = Reply::scores(request, 0.1);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            let candidate = input["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == answer["name"]);
            if candidate.is_some_and(|c| c["path"] == "big.rs" && c["start_line"] == 1) {
                answer["probability"] = json!(0.9);
            }
        }
        reply
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    let deepen: Vec<_> = big_file_requests(&server).into_iter().skip(2).collect();
    assert_eq!(deepen.len(), DEEPEN_JOBS);
    assert!(deepen.iter().all(|lines| lines.len() == DEEPEN_BATCH));
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn deepening_results_are_rechecked_before_output() {
    let root = fixture(300);
    let mut big = String::from("fn behavior_entry() { start(); }\n");
    for n in 1..600 {
        big.push_str(&format!("fn deep_{n:03}() {{}}\n"));
    }
    fs::write(root.path().join("big.rs"), &big).unwrap();
    let path = root.path().join("big.rs");
    let server = Server::new(move |request, _| {
        if is_route(request) {
            return Reply::scores(request, 0.5);
        }
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        let candidates = input["candidates"].as_array().unwrap();
        let big_lines: Vec<u64> = candidates
            .iter()
            .filter(|c| c["path"] == "big.rs")
            .map(|c| c["start_line"].as_u64().unwrap())
            .collect();
        if candidates.len() == 1 && big_lines == [1] {
            return Reply::scores(request, 0.9);
        }
        if big_lines.len() >= 4 {
            fs::write(&path, "rewritten\n").unwrap();
            return Reply::scores(request, 0.9);
        }
        if big_lines.contains(&1) {
            return Reply::nominate(
                request,
                0.1,
                candidates
                    .iter()
                    .find(|c| c["path"] == "big.rs" && c["start_line"] == 1)
                    .unwrap()["name"]
                    .as_str()
                    .unwrap(),
                0.9,
            );
        }
        Reply::scores(request, 0.1)
    })
    .await;
    let report = run(&root, &server, Options::default()).await;
    assert_eq!(report.changed_files, ["big.rs"]);
    assert!(report.results.iter().all(|record| record.path != "big.rs"));
    assert!(!report.coverage.complete);
}

#[test]
fn deepen_plan_orders_by_file_strength_then_distance() {
    let root = fixture(0);
    let lines: String = (1..=600).map(|n| format!("line_{n:03}\n")).collect();
    fs::write(root.path().join("f000.py"), &lines).unwrap();
    fs::write(root.path().join("f001.py"), &lines).unwrap();
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let windows_of = |file: usize| -> Vec<usize> {
        (0..prepared.windows.len())
            .filter(|&index| prepared.windows[index].file == file)
            .collect()
    };
    let (a, b) = (windows_of(0), windows_of(1));
    assert!(a.len() >= 8 && b.len() >= 8);
    // f001 is accepted more strongly, in its middle window; f000 at its first window.
    let probabilities: BTreeMap<usize, Option<f64>> =
        BTreeMap::from([(a[0], Some(0.6)), (b[4], Some(0.9))]);
    let fresh: BTreeSet<usize> = BTreeSet::from([0, 1]);
    let jobs = deepen::plan(
        &prepared,
        "behavior",
        &probabilities,
        &fresh,
        0.5,
        Reservation {
            attempts: 20,
            bytes: 1000,
        },
        Policy {
            max_attempts: 24,
            max_bytes: 768 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    let order: Vec<usize> = jobs
        .iter()
        .flat_map(|job| match &job.purpose {
            Purpose::Source(indices) => indices.clone(),
            _ => panic!("source jobs only"),
        })
        .collect();
    let b_first: Vec<usize> = order
        .iter()
        .copied()
        .take_while(|index| b.contains(index))
        .collect();
    assert_eq!(
        b_first.len(),
        b.len() - 1,
        "the stronger file is read first, all of it"
    );
    assert_eq!(
        &b_first[..2],
        &[b[3], b[5]],
        "nearest to the accepted middle window first"
    );
    assert!(order[b_first.len()..].iter().all(|index| a.contains(index)));
    assert_eq!(order.len(), (a.len() - 1) + (b.len() - 1));
}

#[tokio::test]
async fn initial_pool_follows_the_initial_policy_so_exploration_spans_directories() {
    let root = fixture(0);
    for (dir, count) in [("a", 60), ("b", 40)] {
        fs::create_dir_all(root.path().join(dir)).unwrap();
        for index in 0..count {
            fs::write(
                root.path().join(dir).join(format!("f{index:03}.rs")),
                format!("fn item_{dir}_{index}() {{}}\n"),
            )
            .unwrap();
        }
    }
    let baseline = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "item behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    assert_eq!(baseline.candidate_count(), 64);
    let server = Server::new(|request, _| {
        if is_route(request) {
            return Reply::status(400);
        }
        Reply::scores(request, 0.1)
    })
    .await;
    run(&root, &server, Options::default()).await;
    let sent: BTreeSet<usize> = request_source_ids(&server.bodies()).into_iter().collect();
    let expected: BTreeSet<usize> = baseline.selected[..56].iter().copied().collect();
    assert_eq!(
        sent, expected,
        "the initial pass reads the compact pool's first 56 windows"
    );
    let from_b = sent
        .iter()
        .filter(|&&index| baseline.windows[index].file >= 60)
        .count();
    assert!(from_b >= 20, "only {from_b} windows from b/");
}

#[tokio::test]
async fn sixteen_requests_run_concurrently() {
    let server = Server::new(|request, _| {
        let mut reply = Reply::scores(request, 0.1);
        reply.delay = Duration::from_millis(25);
        reply
    })
    .await;
    run(&fixture(256), &server, Options::thorough()).await;
    assert_eq!(server.peak.load(Ordering::SeqCst), CONCURRENCY);
}

#[test]
fn deepen_plan_includes_implicated_files_after_accepted_ones() {
    let root = fixture(0);
    let lines: String = (1..=600).map(|n| format!("line_{n:03}\n")).collect();
    for name in ["f000.py", "f001.py", "f002.py"] {
        fs::write(root.path().join(name), &lines).unwrap();
    }
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let windows_of = |file: usize| -> Vec<usize> {
        (0..prepared.windows.len())
            .filter(|&index| prepared.windows[index].file == file)
            .collect()
    };
    let (a, b, c) = (windows_of(0), windows_of(1), windows_of(2));
    // f001 accepted at 0.9; f000 implicated at 0.3 (its strongest window below the threshold); f002 at 0.1 (below the floor).
    let probabilities: BTreeMap<usize, Option<f64>> =
        BTreeMap::from([(a[2], Some(0.3)), (b[0], Some(0.9)), (c[0], Some(0.1))]);
    let fresh: BTreeSet<usize> = BTreeSet::from([1]);
    let jobs = deepen::plan(
        &prepared,
        "behavior",
        &probabilities,
        &fresh,
        0.5,
        Reservation {
            attempts: 20,
            bytes: 1000,
        },
        Policy {
            max_attempts: 60,
            max_bytes: 2048 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    let order: Vec<usize> = jobs
        .iter()
        .flat_map(|job| match &job.purpose {
            Purpose::Source(indices) => indices.clone(),
            _ => panic!("source jobs only"),
        })
        .collect();
    let b_count = b.len() - 1;
    assert_eq!(
        order.len(),
        (a.len() - 1) + b_count,
        "both implicated files, nothing else"
    );
    assert!(
        order[..b_count].iter().all(|index| b.contains(index)),
        "accepted file first"
    );
    assert_eq!(
        order[b_count], a[1],
        "then the implicated file, nearest to its strongest window first"
    );
    assert_eq!(order[b_count + 1], a[3]);
    assert!(
        order.iter().all(|index| !c.contains(index)),
        "files below the floor are not read"
    );
}

#[test]
fn deepen_plan_sweeps_the_files_the_query_words_point_at_once_something_is_accepted() {
    let root = fixture(0);
    let plain: String = (1..=600).map(|n| format!("line_{n:03}\n")).collect();
    let mut lexical = plain.clone();
    lexical.push_str("fn behavior_entry() { behavior(); }\n");
    fs::write(root.path().join("f000.py"), &plain).unwrap();
    fs::write(root.path().join("f001.py"), &lexical).unwrap();
    fs::write(root.path().join("f002.py"), "fn item() {}\n").unwrap();
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let windows_of = |file: usize| -> Vec<usize> {
        (0..prepared.windows.len())
            .filter(|&index| prepared.windows[index].file == file)
            .collect()
    };
    let (b, c) = (windows_of(1), windows_of(2));
    let plan =
        |probabilities: BTreeMap<usize, Option<f64>>, fresh: BTreeSet<usize>| -> Vec<usize> {
            deepen::plan(
                &prepared,
                "behavior",
                &probabilities,
                &fresh,
                0.5,
                Reservation {
                    attempts: 20,
                    bytes: 1000,
                },
                Policy {
                    max_attempts: 60,
                    max_bytes: 2048 * 1024,
                    attempt_timeout: Duration::from_secs(5),
                },
                &mut || Control::Continue,
            )
            .iter()
            .flat_map(|job| match &job.purpose {
                Purpose::Source(indices) => indices.clone(),
                _ => panic!("source jobs only"),
            })
            .collect()
        };
    // Nothing accepted: the query-word tier stays off, so a no-answer search reads nothing more.
    let nothing = plan(
        BTreeMap::from([(*b.last().unwrap(), Some(0.0))]),
        BTreeSet::new(),
    );
    assert!(nothing.is_empty());
    // The one-window file f002 is accepted (too short to deepen itself); f001 mentions the query
    // word and its last window was read and rejected at 0.0.
    let order = plan(
        BTreeMap::from([(c[0], Some(0.9)), (*b.last().unwrap(), Some(0.0))]),
        BTreeSet::from([2]),
    );
    assert_eq!(
        order.len(),
        b.len() - 1,
        "every unread window of the lexical file, nothing from the other"
    );
    assert!(order.iter().all(|index| b.contains(index)));
    assert_eq!(
        order[0],
        b[b.len() - 2],
        "nearest to the window that mentions the query first"
    );
}

#[test]
fn deepen_plan_sweeps_files_that_declare_what_accepted_code_calls() {
    let root = fixture(0);
    let mut caller = String::from("fn entry() {\n    helper_thing();\n}\n");
    caller.push_str(
        &(1..=400)
            .map(|n| format!("line_{n:03}\n"))
            .collect::<String>(),
    );
    let mut callee: String = (1..=300).map(|n| format!("other_{n:03}\n")).collect();
    callee.push_str("fn helper_thing() {}\n");
    callee.push_str(
        &(1..=300)
            .map(|n| format!("more_{n:03}\n"))
            .collect::<String>(),
    );
    let unrelated: String = (1..=600).map(|n| format!("alone_{n:03}\n")).collect();
    fs::write(root.path().join("f000.rs"), &caller).unwrap();
    fs::write(root.path().join("f001.rs"), &callee).unwrap();
    fs::write(root.path().join("f002.rs"), &unrelated).unwrap();
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let windows_of = |file: usize| -> Vec<usize> {
        (0..prepared.windows.len())
            .filter(|&index| prepared.windows[index].file == file)
            .collect()
    };
    let (a, b, c) = (windows_of(0), windows_of(1), windows_of(2));
    let declaring = *b
        .iter()
        .find(|&&index| {
            let w = &prepared.windows[index];
            w.start_line <= 301 && 301 <= w.end_line
        })
        .unwrap();
    // Only the caller's first window is accepted; f001 has never been read and shares no query word.
    let jobs = deepen::plan(
        &prepared,
        "behavior",
        &BTreeMap::from([(a[0], Some(0.9))]),
        &BTreeSet::from([0]),
        0.5,
        Reservation {
            attempts: 20,
            bytes: 1000,
        },
        Policy {
            max_attempts: 60,
            max_bytes: 2048 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    let order: Vec<usize> = jobs
        .iter()
        .flat_map(|job| match &job.purpose {
            Purpose::Source(indices) => indices.clone(),
            _ => panic!("source jobs only"),
        })
        .collect();
    let a_unread = a.len() - 1;
    assert!(
        order[..a_unread].iter().all(|index| a.contains(index)),
        "the accepted file first"
    );
    let rest = &order[a_unread..];
    assert_eq!(
        rest.len(),
        b.len(),
        "then every window of the file that declares the callee"
    );
    assert!(rest.iter().all(|index| b.contains(index)));
    assert_eq!(rest[0], declaring, "the declaring window first");
    assert!(
        order.iter().all(|index| !c.contains(index)),
        "unrelated files are not read"
    );
}

#[test]
fn deepen_plan_skips_files_already_found_stale() {
    let root = fixture(0);
    let lines: String = (1..=600).map(|n| format!("line_{n:03}\n")).collect();
    fs::write(root.path().join("f000.py"), &lines).unwrap();
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let a: Vec<usize> = (0..prepared.windows.len()).collect();
    // Accepted at 0.9 but the file failed its freshness recheck (not in `fresh`); another window at 0.3.
    let probabilities: BTreeMap<usize, Option<f64>> =
        BTreeMap::from([(a[0], Some(0.9)), (a[2], Some(0.3))]);
    let jobs = deepen::plan(
        &prepared,
        "behavior",
        &probabilities,
        &BTreeSet::new(),
        0.5,
        Reservation {
            attempts: 20,
            bytes: 1000,
        },
        Policy {
            max_attempts: 60,
            max_bytes: 2048 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    assert!(jobs.is_empty(), "a stale file is never deepened");
}

#[test]
fn deepen_callee_tier_prefers_rare_names_and_caps_its_files() {
    let root = fixture(0);
    // The accepted window calls a common name and a rare one; a huge file declares the common name
    // and sorts first by path, a small-but-deepenable file declares the rare one.
    let mut caller =
        String::from("fn entry() {\n    let t = Thing::new();\n    helper_thing();\n}\n");
    caller.push_str(
        &(1..=400)
            .map(|n| format!("line_{n:03}\n"))
            .collect::<String>(),
    );
    let mut common = String::from("pub fn new() -> Self { Self }\n");
    common.push_str(
        &(1..=2000)
            .map(|n| format!("common_{n:04}\n"))
            .collect::<String>(),
    );
    let mut rare: String = (1..=300).map(|n| format!("other_{n:03}\n")).collect();
    rare.push_str("fn helper_thing() {}\n");
    rare.push_str(
        &(1..=300)
            .map(|n| format!("more_{n:03}\n"))
            .collect::<String>(),
    );
    // `new` is also declared by a second file, so it is the less rare name.
    let also_common = String::from("pub fn new() -> Self { Self }\n")
        + &(1..=400)
            .map(|n| format!("c2_{n:03}\n"))
            .collect::<String>();
    fs::write(root.path().join("a.rs"), &common).unwrap();
    fs::write(root.path().join("b.rs"), &also_common).unwrap();
    fs::write(root.path().join("m.rs"), &caller).unwrap();
    fs::write(root.path().join("x.rs"), &rare).unwrap();
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let file_of = |name: &str| {
        prepared
            .snapshot
            .files()
            .iter()
            .position(|f| f.path() == name)
            .unwrap()
    };
    let windows_of = |file: usize| -> Vec<usize> {
        (0..prepared.windows.len())
            .filter(|&index| prepared.windows[index].file == file)
            .collect()
    };
    let m = windows_of(file_of("m.rs"));
    let x = windows_of(file_of("x.rs"));
    let jobs = deepen::plan(
        &prepared,
        "behavior",
        &BTreeMap::from([(m[0], Some(0.9))]),
        &BTreeSet::from([file_of("m.rs")]),
        0.5,
        Reservation {
            attempts: 20,
            bytes: 1000,
        },
        Policy {
            max_attempts: 60,
            max_bytes: 2048 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    let order: Vec<usize> = jobs
        .iter()
        .flat_map(|job| match &job.purpose {
            Purpose::Source(indices) => indices.clone(),
            _ => panic!("source jobs only"),
        })
        .collect();
    let after_m: Vec<usize> = order
        .iter()
        .copied()
        .filter(|index| !m.contains(index))
        .collect();
    assert!(
        x.iter().all(|index| after_m.contains(index)),
        "the file declaring the rare name is swept in full"
    );
    assert!(
        after_m[..x.len()].iter().all(|index| x.contains(index)),
        "and it comes before the common-name files: {after_m:?} x {x:?}"
    );
}

#[test]
fn deepen_common_callee_names_carry_no_signal_and_leave_the_query_word_tier_intact() {
    let root = fixture(0);
    let mut caller = String::from("fn entry() {\n    let t = Thing::new();\n}\n");
    caller.push_str(
        &(1..=400)
            .map(|n| format!("line_{n:03}\n"))
            .collect::<String>(),
    );
    for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        let text = String::from("pub fn new() -> Self { Self }\n")
            + &(1..=2000)
                .map(|n| format!("{name}_{n:04}\n"))
                .collect::<String>();
        fs::write(root.path().join(name), text).unwrap();
    }
    let mut lexical: String = (1..=600).map(|n| format!("z_{n:03}\n")).collect();
    lexical.push_str("fn behavior_entry() { behavior(); }\n");
    fs::write(root.path().join("m.rs"), &caller).unwrap();
    fs::write(root.path().join("z.rs"), &lexical).unwrap();
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let file_of = |name: &str| {
        prepared
            .snapshot
            .files()
            .iter()
            .position(|f| f.path() == name)
            .unwrap()
    };
    let windows_of = |file: usize| -> Vec<usize> {
        (0..prepared.windows.len())
            .filter(|&index| prepared.windows[index].file == file)
            .collect()
    };
    let m = windows_of(file_of("m.rs"));
    let z = windows_of(file_of("z.rs"));
    let jobs = deepen::plan(
        &prepared,
        "behavior",
        &BTreeMap::from([(m[0], Some(0.9))]),
        &BTreeSet::from([file_of("m.rs")]),
        0.5,
        Reservation {
            attempts: 20,
            bytes: 1000,
        },
        Policy {
            max_attempts: 60,
            max_bytes: 2048 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    let order: Vec<usize> = jobs
        .iter()
        .flat_map(|job| match &job.purpose {
            Purpose::Source(indices) => indices.clone(),
            _ => panic!("source jobs only"),
        })
        .collect();
    let after_m: Vec<usize> = order
        .iter()
        .copied()
        .filter(|index| !m.contains(index))
        .collect();
    assert_eq!(
        after_m.len(),
        z.len(),
        "the query-word file is swept and the four `new` declarers are not: {after_m:?}"
    );
    assert!(after_m.iter().all(|index| z.contains(index)));
}

#[test]
fn deepen_files_the_callee_tier_collects_but_does_not_sweep_stay_eligible_for_the_query_word_tier()
{
    let root = fixture(0);
    let mut caller = String::from(
        "fn entry() {\n    alpha_fn();\n    beta_fn();\n    gamma_fn();\n    delta_fn();\n}\n",
    );
    caller.push_str(
        &(1..=400)
            .map(|n| format!("line_{n:03}\n"))
            .collect::<String>(),
    );
    for (name, declared) in [
        ("a.rs", "alpha_fn"),
        ("b.rs", "beta_fn"),
        ("c.rs", "gamma_fn"),
    ] {
        let text = format!("fn {declared}() {{}}\n")
            + &(1..=600)
                .map(|n| format!("{declared}_{n:03}\n"))
                .collect::<String>();
        fs::write(root.path().join(name), text).unwrap();
    }
    // The fourth callee file also carries the query word, so it is the top query-word file.
    let mut d = String::from("fn delta_fn() { behavior(); }\n");
    d.push_str(
        &(1..=600)
            .map(|n| format!("delta_{n:03}\n"))
            .collect::<String>(),
    );
    fs::write(root.path().join("d.rs"), &d).unwrap();
    fs::write(root.path().join("m.rs"), &caller).unwrap();
    let prepared = prepare_with_policy(
        &Source::open(root.path()).unwrap(),
        "behavior",
        test_policy(&Options::default()),
        &mut || Control::Continue,
    );
    let file_of = |name: &str| {
        prepared
            .snapshot
            .files()
            .iter()
            .position(|f| f.path() == name)
            .unwrap()
    };
    let windows_of = |file: usize| -> Vec<usize> {
        (0..prepared.windows.len())
            .filter(|&index| prepared.windows[index].file == file)
            .collect()
    };
    let m = windows_of(file_of("m.rs"));
    let d_windows = windows_of(file_of("d.rs"));
    let jobs = deepen::plan(
        &prepared,
        "behavior",
        &BTreeMap::from([(m[0], Some(0.9))]),
        &BTreeSet::from([file_of("m.rs")]),
        0.5,
        Reservation {
            attempts: 20,
            bytes: 1000,
        },
        Policy {
            max_attempts: 60,
            max_bytes: 2048 * 1024,
            attempt_timeout: Duration::from_secs(5),
        },
        &mut || Control::Continue,
    );
    let order: Vec<usize> = jobs
        .iter()
        .flat_map(|job| match &job.purpose {
            Purpose::Source(indices) => indices.clone(),
            _ => panic!("source jobs only"),
        })
        .collect();
    assert!(
        d_windows.iter().all(|index| order.contains(index)),
        "d.rs is swept by the query-word tier although the callee tier collected it: {order:?} d {d_windows:?}"
    );
}

#[tokio::test]
async fn output_limit_drops_the_weakest_records_first_in_json_and_text() {
    // Three big files; the middle one in output order is the weakest and must be the one dropped.
    let root = fixture(0);
    let heavy = "\"\\ab\n".repeat(6_000);
    for name in ["a/first.rs", "b/second.rs", "c/third.rs"] {
        let path = root.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &heavy).unwrap();
    }
    let server = Server::new(|request, _| {
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        let mut reply = Reply::scores(request, 0.9);
        for answer in reply.body["answers"].as_array_mut().unwrap() {
            let candidate = input["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == answer["name"]);
            if candidate.is_some_and(|c| c["path"].as_str().unwrap().starts_with("b/")) {
                answer["probability"] = json!(0.6);
            }
        }
        reply
    })
    .await;
    let mut json_report = run(&root, &server, Options::thorough()).await;
    assert_eq!(json_report.results.len(), 3);
    let bytes = json_report.encode_json().unwrap();
    assert!(bytes.len() <= MAX_OUTPUT_BYTES);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let paths: Vec<&str> = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert!(value["output_truncated"].as_bool().unwrap());
    assert!(
        !paths.contains(&"b/second.rs"),
        "the 0.6 record goes first: {paths:?}"
    );
    assert!(!paths.is_empty());
    let mut text_report = run(&root, &server, Options::thorough()).await;
    assert_eq!(text_report.results.len(), 3);
    let text = String::from_utf8(text_report.encode_text()).unwrap();
    assert!(text.len() <= MAX_OUTPUT_BYTES);
    assert!(
        !text.contains("b/second.rs"),
        "results {:?} truncated {} omitted {} len {}",
        text_report
            .results
            .iter()
            .map(|r| (r.path.clone(), r.probability, r.excerpt.len()))
            .collect::<Vec<_>>(),
        text_report.output_truncated,
        text_report.omitted_results,
        text.len()
    );
}
