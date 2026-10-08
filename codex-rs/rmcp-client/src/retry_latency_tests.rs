use super::*;
use axum::Router;
use axum::body::Body;
use axum::routing::any;

#[tokio::test]
async fn diagnostic_deadlines_preserve_status_and_protocol_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/{case}", any(
        |axum::extract::Path(case): axum::extract::Path<String>| async move {
            let (status, content_type, body) = match case.as_str() {
                "rpc-error" | "tool-error" => (
                    StatusCode::BAD_REQUEST, JSON_MIME_TYPE,
                    Body::from_stream(stream::pending::<Result<Bytes, io::Error>>()),
                ),
                "invalid-broken" => (
                    StatusCode::OK, "text/plain",
                    Body::from_stream(stream::once(async { Ok::<_, io::Error>(Bytes::from_static(b"x")) })
                        .chain(stream::once(async {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Err::<Bytes, _>(io::Error::other("broken preview"))
                        }))),
                ),
                "slow-rpc" => (
                    StatusCode::BAD_REQUEST, JSON_MIME_TYPE,
                    Body::from_stream(stream::once(async {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<_, io::Error>(Bytes::from_static(
                            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid arguments"}}"#,
                        ))
                    })),
                ),
                _ => (StatusCode::OK, "text/plain",
                    Body::from_stream(stream::pending::<Result<Bytes, io::Error>>())),
            };
            (status, [(CONTENT_TYPE, content_type)], body)
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let adapter = StreamableHttpClientAdapter::new(
        Arc::new(codex_exec_server::ReqwestHttpClient), HeaderMap::new(), None,
    );
    for case in ["rpc-error", "tool-error", "invalid", "invalid-broken", "slow-rpc"] {
        let result = tokio::time::timeout(Duration::from_secs(2), adapter.post_message(
            format!("http://{address}/{case}").into(),
            serde_json::from_value(serde_json::json!({
                "jsonrpc":"2.0", "id":1,
                "method": if case == "tool-error" { "tools/call" } else { "tools/list" },
                "params": { "name":"fixture", "arguments":{} }
            })).unwrap(),
            None, None, HashMap::new(),
        )).await.expect("diagnostics must not consume the entire operation timeout");
        match (case, result) {
            ("rpc-error" | "tool-error", Err(StreamableHttpError::Client(
                StreamableHttpClientAdapterError::UnexpectedHttpStatus { status, body_preview, .. }
            ))) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(body_preview.contains("timed out"));
            }
            ("invalid" | "invalid-broken", Err(StreamableHttpError::UnexpectedContentType(Some(message)))) => {
                assert!(message.contains("text/plain"));
                assert!(message.contains(if case == "invalid" { "timed out" } else { "unavailable" }));
            }
            ("slow-rpc", Ok(StreamableHttpPostResponse::Json(JsonRpcMessage::Error(error), _))) => {
                assert_eq!(error.error.message, "invalid arguments");
            }
            _ => panic!("unexpected diagnostic classification for {case}"),
        }
    }
    server.abort();
    let _ = server.await;
}

/// Local fault-injection benchmark, deliberately excluded from ordinary tests.
/// Run with nextest's `--success-output immediate` to display the measurements.
#[tokio::test]
#[ignore]
async fn stalled_diagnostics_benchmark() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/{case}", any(
        |axum::extract::Path(case): axum::extract::Path<String>| async move {
            let (status, content_type) = match case.as_str() {
                "retryable" => (StatusCode::SERVICE_UNAVAILABLE, "text/plain"),
                "rpc-error" => (StatusCode::BAD_REQUEST, JSON_MIME_TYPE),
                "tool-error" => (StatusCode::SERVICE_UNAVAILABLE, JSON_MIME_TYPE),
                _ => (StatusCode::OK, "text/plain"),
            };
            (status, [(CONTENT_TYPE, content_type)],
                Body::from_stream(stream::pending::<Result<Bytes, io::Error>>()))
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let adapter = StreamableHttpClientAdapter::new(
        Arc::new(codex_exec_server::ReqwestHttpClient), HeaderMap::new(), None,
    );
    let mut measurements = Vec::new();
    for case in ["retryable", "rpc-error", "tool-error", "invalid-type"] {
        let mut samples_ms = Vec::new();
        let mut outer_timeouts = 0;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(Duration::from_millis(1500), adapter.post_message(
                format!("http://{address}/{case}").into(),
                serde_json::from_value(serde_json::json!({
                    "jsonrpc":"2.0", "id":1,
                    "method": if case == "tool-error" { "tools/call" } else { "tools/list" },
                    "params": { "name": "fixture", "arguments": {} }
                })).unwrap(),
                None, None, HashMap::new(),
            )).await;
            samples_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            outer_timeouts += usize::from(result.is_err());
            assert!(!matches!(result, Ok(Ok(_))), "fault must not become a success");
        }
        measurements.push(serde_json::json!({
            "case":case, "samples_ms":samples_ms, "outer_timeouts":outer_timeouts
        }));
    }
    server.abort();
    let _ = server.await;
    let report = serde_json::to_string_pretty(&measurements).unwrap();
    println!("{report}");
}
