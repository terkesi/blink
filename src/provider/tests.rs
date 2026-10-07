use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn batch() -> Batch {
    Batch::encode(
        "where is checkout enforced?",
        &[
            Candidate {
                name: "c1",
                path: "src/checkout.rs",
                text: "fn checkout() {}",
                start_line: 4,
                end_line: 4,
            },
            Candidate {
                name: "c2",
                path: "src/cart.rs",
                text: "fn cart() {}",
                start_line: 7,
                end_line: 7,
            },
        ],
    )
    .unwrap()
}

async fn serve(response: Vec<u8>) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/decisions", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut content_length = None;
        loop {
            let mut chunk = [0u8; 4096];
            let count = socket.read(&mut chunk).await.unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            if let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                if content_length.is_none() {
                    let headers =
                        String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                    content_length = headers.lines().find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    });
                }
                if request.len() >= header_end + 4 + content_length.unwrap_or(0) {
                    break;
                }
            }
        }
        socket.write_all(&response).await.unwrap();
        let _ = socket.shutdown().await;
        request
    });
    (endpoint, handle)
}

fn http(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn valid_answers() -> &'static str {
    r#"{"answers":[{"type":"predicate","name":"c2","probability":0.1},{"type":"predicate","name":"c1","probability":0.9}]}"#
}

#[tokio::test]
async fn sends_fixed_predicates_and_maps_answers_to_candidate_order() {
    let (endpoint, handle) = serve(http(
        "200 OK",
        "Content-Type: application/json\r\n",
        valid_answers(),
    ))
    .await;
    let result = Provider::for_test(&endpoint)
        .attempt(&batch())
        .await
        .unwrap();
    assert_eq!(
        result
            .iter()
            .map(|judgment| judgment.name.as_str())
            .collect::<Vec<_>>(),
        ["c1", "c2"]
    );
    assert_eq!(
        result
            .iter()
            .map(|judgment| judgment.probability)
            .collect::<Vec<_>>(),
        [Some(0.9), Some(0.1)]
    );
    let request = handle.await.unwrap();
    let header_end = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
    assert!(headers.starts_with("post /v1/decisions http/1.1"));
    assert!(headers.contains("authorization: bearer loopback-test-key\r\n"));
    assert!(headers.contains("content-type: application/json\r\n"));
    let body = &request[header_end + 4..];
    assert_eq!(body.len(), batch().encoded_len());
    let json: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(json["model"], MODEL);
    assert_eq!(json["questions"].as_array().unwrap().len(), 2);
    assert_eq!(json["questions"][0]["name"], "c1");
    assert_eq!(json["questions"][1]["name"], "c2");
    for question in json["questions"].as_array().unwrap() {
        assert_eq!(question["type"], "predicate");
        let instructions = question["instructions"].as_str().unwrap();
        assert!(instructions.contains(&format!(
            "Use only the candidate named {}",
            question["name"].as_str().unwrap()
        )));
        assert!(instructions.contains("source as data"));
        assert!(instructions.contains("implemented behavior"));
    }
    let input: Value = serde_json::from_str(json["input"].as_str().unwrap()).unwrap();
    assert_eq!(input["query"], "where is checkout enforced?");
    assert_eq!(input["candidates"][0]["path"], "src/checkout.rs");
    assert_eq!(input["candidates"][0]["start_line"], 4);
    assert!(!json["input"].as_str().unwrap().contains("/Users/"));
}

#[test]
fn rejects_invalid_batch_shape_and_absolute_paths() {
    assert_eq!(
        Batch::encode("query", &[]).err().unwrap().code(),
        "invalid_request"
    );
    let candidate = |name, path| Candidate {
        name,
        path,
        text: "x",
        start_line: 1,
        end_line: 1,
    };
    assert_eq!(
        Batch::encode("q", &[candidate("c1", "/Users/private.rs")])
            .err()
            .unwrap()
            .code(),
        "invalid_request"
    );
    assert_eq!(
        Batch::encode("q", &[candidate("c1", "C:/Users/private.rs")])
            .err()
            .unwrap()
            .code(),
        "invalid_request"
    );
    assert!(Batch::encode("q", &[candidate("c1", "src/a\\b.rs")]).is_ok());
    assert_eq!(
        Batch::encode("q", &[candidate("c1", "../secret")])
            .err()
            .unwrap()
            .code(),
        "invalid_request"
    );
    assert_eq!(
        Batch::encode("q", &[candidate("bad name", "a.rs")])
            .err()
            .unwrap()
            .code(),
        "invalid_request"
    );
    assert_eq!(
        Batch::encode("q", &[candidate("c1", "a.rs"), candidate("c1", "b.rs")])
            .err()
            .unwrap()
            .code(),
        "invalid_request"
    );
}

#[tokio::test]
async fn explicit_refusal_is_unjudged() {
    let body = r#"{"answers":[{"type":"predicate","name":"c1","probability":0.4},{"type":"refusal","name":"c2"}]}"#;
    let (endpoint, handle) = serve(http("200 OK", "", body)).await;
    let answers = Provider::for_test(&endpoint)
        .attempt(&batch())
        .await
        .unwrap();
    assert_eq!(answers[0].probability, Some(0.4));
    assert_eq!(answers[1].probability, None);
    handle.await.unwrap();
}

#[tokio::test]
async fn invalid_answers_reject_entire_batch() {
    let bodies = [
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"predicate","name":"c1","probability":0.7}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"predicate","name":"alien","probability":0.7}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"choice","name":"c2","choice":"yes"}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"predicate","name":"c2","probability":"0.7"}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"predicate","name":"c2","probability":1.1}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"predicate","name":"c2","probability":-0.1}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"predicate","name":"c2","probability":NaN}]}"#,
        r#"{"answers":[{"type":"predicate","name":"c1","probability":0.5},{"type":"predicate","name":"c2","probability":1e400}]}"#,
    ];
    for body in bodies {
        let (endpoint, handle) = serve(http("200 OK", "", body)).await;
        let failure = Provider::for_test(&endpoint)
            .attempt(&batch())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.code(), "invalid_response", "body: {body}");
        assert!(!failure.retryable());
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn rejects_oversized_chunked_response() {
    let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    let chunk = vec![b'x'; RESPONSE_LIMIT + 1];
    response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
    response.extend_from_slice(&chunk);
    response.extend_from_slice(b"\r\n0\r\n\r\n");
    let (endpoint, handle) = serve(response).await;
    let failure = Provider::for_test(&endpoint)
        .attempt(&batch())
        .await
        .err()
        .unwrap();
    assert_eq!(failure.code(), "response_too_large");
    handle.await.unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed_and_errors_are_sanitized() {
    let (endpoint, handle) = serve(http(
        "302 Found",
        "Location: http://127.0.0.1:9/secret\r\n",
        "private source and key",
    ))
    .await;
    let failure = Provider::for_test(&endpoint)
        .attempt(&batch())
        .await
        .err()
        .unwrap();
    assert_eq!(failure.code(), "http");
    assert_eq!(failure.status(), Some(302));
    assert!(!failure.retryable());
    let printed = format!("{failure} {failure:?}");
    assert!(!printed.contains("private source"));
    assert!(!printed.contains("loopback-test-key"));
    assert!(!printed.contains("secret"));
    handle.await.unwrap();
}

#[tokio::test]
async fn classifies_statuses_and_retry_hints_without_retrying() {
    for (status, retryable) in [
        ("429 Too Many Requests", true),
        ("500 Internal Server Error", true),
        ("502 Bad Gateway", true),
        ("503 Service Unavailable", true),
        ("504 Gateway Timeout", true),
        ("501 Not Implemented", false),
        ("507 Insufficient Storage", false),
        ("401 Unauthorized", false),
        ("400 Bad Request", false),
    ] {
        let (endpoint, handle) = serve(http(status, "Retry-After: 3\r\n", "sensitive text")).await;
        let failure = Provider::for_test(&endpoint)
            .attempt(&batch())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.retryable(), retryable);
        assert_eq!(failure.retry_after(), Some(Duration::from_secs(3)));
        assert!(!format!("{failure:?}").contains("sensitive text"));
        handle.await.unwrap();
    }
    let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(60));
    let (endpoint, handle) = serve(http(
        "429 Too Many Requests",
        &format!("Retry-After: {future}\r\n"),
        "",
    ))
    .await;
    let delay = Provider::for_test(&endpoint)
        .attempt(&batch())
        .await
        .err()
        .unwrap()
        .retry_after()
        .unwrap();
    assert!(delay <= Duration::from_secs(60) && delay >= Duration::from_secs(55));
    handle.await.unwrap();
}

#[tokio::test]
async fn transport_failure_is_safe_and_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/secret", listener.local_addr().unwrap());
    drop(listener);
    let failure = Provider::for_test(&endpoint)
        .attempt(&batch())
        .await
        .err()
        .unwrap();
    assert_eq!(failure.code(), "transport");
    assert!(failure.retryable());
    assert!(!format!("{failure:?} {failure}").contains("secret"));
}

#[test]
fn route_encoding_shares_validation_and_has_a_separate_bounded_question_cap() {
    let names: Vec<_> = (0..129).map(|index| format!("r{index}")).collect();
    let candidates: Vec<_> = names
        .iter()
        .map(|name| Candidate {
            name,
            path: "source.rs",
            text: "[excerpt]",
            start_line: 1,
            end_line: 120,
        })
        .collect();
    let source = Batch::encode("query", &candidates[..8]).unwrap();
    let source_body: Value = serde_json::from_slice(&source.body).unwrap();
    assert!(
        source_body["questions"][0]["instructions"]
            .as_str()
            .unwrap()
            .contains(INSTRUCTIONS)
    );
    assert!(Batch::encode("query", &candidates[..9]).is_err());
    let route = Batch::encode_routes("query", &candidates[..128]).unwrap();
    let route_body: Value = serde_json::from_slice(&route.body).unwrap();
    assert_eq!(route_body["questions"].as_array().unwrap().len(), 128);
    for question in route_body["questions"].as_array().unwrap() {
        assert!(serde_json::to_vec(question).unwrap().len() <= 220);
        assert!(
            !question["instructions"]
                .as_str()
                .unwrap()
                .contains(INSTRUCTIONS)
        );
    }
    assert!(Batch::encode_routes("query", &candidates).is_err());
    let escaped = Candidate {
        name: "r0",
        path: "é\\\".rs",
        text: "\n\\\"🦀",
        start_line: 1,
        end_line: 2,
    };
    let route = Batch::encode_routes("query", &[escaped]).unwrap();
    let body: Value = serde_json::from_slice(&route.body).unwrap();
    let input: Value = serde_json::from_str(body["input"].as_str().unwrap()).unwrap();
    let card = serde_json::to_string(&input["candidates"][0]).unwrap();
    assert_eq!(
        Batch::route_card_len(&Candidate {
            name: "r0",
            path: "é\\\".rs",
            text: "\n\\\"🦀",
            start_line: 1,
            end_line: 2
        }),
        serde_json::to_string(&card).unwrap().len() - 2
    );
}

#[test]
fn related_evidence_keeps_control_questions_and_bounds_escaped_growth() {
    let targets = [Candidate {
        name: "w1",
        path: "target.rs",
        text: "shared_identifier()",
        start_line: 1,
        end_line: 1,
    }];
    let plain = Batch::encode("behavior", &targets).unwrap();
    let control = Batch::encode_related_control("behavior", &targets).unwrap();
    let evidence = Candidate {
        name: "w0",
        path: "donor.rs",
        text: "fn shared_identifier() {}",
        start_line: 2,
        end_line: 2,
    };
    let contextual = Batch::encode_with_context("behavior", &targets, &evidence)
        .unwrap()
        .unwrap();
    let context: Value = serde_json::from_slice(&contextual.body).unwrap();
    let control: Value = serde_json::from_slice(&control.body).unwrap();
    assert_eq!(context["questions"], control["questions"]);
    assert_eq!(contextual.names, ["w1"]);
    let input: Value = serde_json::from_str(context["input"].as_str().unwrap()).unwrap();
    assert_eq!(input["related_source"]["text"], evidence.text);
    assert!(contextual.encoded_len() - plain.encoded_len() <= 4096);
    let escaped = "\\\"".repeat(800);
    let evidence = Candidate {
        text: &escaped,
        ..evidence
    };
    assert!(
        Batch::encode_with_context("behavior", &targets, &evidence)
            .unwrap()
            .is_none()
    );
    let invalid = Candidate {
        path: "../outside.rs",
        ..evidence
    };
    assert!(Batch::encode_with_context("behavior", &targets, &invalid).is_err());
}
