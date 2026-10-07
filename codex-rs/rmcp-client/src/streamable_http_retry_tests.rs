use std::any::TypeId;

use codex_exec_server::ExecServerError;
use http::StatusCode;
use pretty_assertions::assert_eq;
use rmcp::transport::DynamicTransportError;
use rmcp::transport::streamable_http_client::StreamableHttpError;

use crate::http_client_adapter::StreamableHttpClientAdapterError;

use super::*;

#[tokio::test]
async fn recovery_failure_is_shared_only_with_existing_waiters() -> anyhow::Result<()> {
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};
    use wiremock::matchers::{method, path};
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&attempts);
    Mock::given(method("POST")).and(path("/mcp")).respond_with(move |request: &Request| {
        let body: serde_json::Value = request.body_json().unwrap();
        match body["method"].as_str() {
            Some("initialize") if count.fetch_add(1, Ordering::SeqCst) == 0 =>
                ResponseTemplate::new(200).insert_header("mcp-session-id", "original").set_body_json(serde_json::json!({
                    "jsonrpc":"2.0", "id":body["id"], "result": {
                        "protocolVersion":body["params"]["protocolVersion"], "capabilities":{"tools":{}},
                        "serverInfo":{"name":"recovery-test","version":"1"}
                    }
                })),
            Some("initialize") => ResponseTemplate::new(503).set_body_string("offline"),
            Some("notifications/initialized") => ResponseTemplate::new(202),
            _ => ResponseTemplate::new(404),
        }
    }).mount(&server).await;
    let home = tempfile::tempdir()?;
    let client = RmcpClient::new_streamable_http_client(
        "recovery-test", home.path().to_path_buf(), &format!("{}/mcp", server.uri()),
        Some("fixture".into()), None, None,
        codex_config::types::OAuthCredentialsStoreMode::File,
        codex_config::types::AuthKeyringBackendKind::default(),
        Arc::new(codex_exec_server::ReqwestHttpClient), None,
    ).await?;
    client.initialize(
        rmcp::model::InitializeRequestParams::new(Default::default(), rmcp::model::Implementation::new("test", "1")),
        Some(Duration::from_secs(5)),
        Box::new(|_, _| async { unreachable!() }.boxed()), Box::new(|_| async {}.boxed()),
    ).await?;
    let outcomes = futures::future::join_all((0..8).map(|_| client.list_tools(None, Some(Duration::from_secs(5))))).await;
    assert!(outcomes.iter().all(Result::is_err));
    assert_eq!(attempts.load(Ordering::SeqCst), 4, "one initial handshake plus one three-attempt recovery episode");
    let errors: Vec<_> = outcomes.into_iter().map(|result| format!("{:#}", result.unwrap_err())).collect();
    assert!(errors.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(client.list_tools(None, Some(Duration::from_secs(5))).await.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 7, "a later caller can start a new episode");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn retry_advice_is_preserved_through_wrappers_and_deadline() {
    let error = rmcp::service::ClientInitializeError::TransportError {
        error: DynamicTransportError::from_parts("streamable_http", TypeId::of::<()>(), Box::new(
            StreamableHttpError::Client(StreamableHttpClientAdapterError::UnexpectedHttpStatus {
                status: StatusCode::TOO_MANY_REQUESTS, body_preview: "busy".into(),
                retry_after: Some(Duration::from_secs(2)),
            })
        )), context: "send initialize request".into(),
    };
    let error = anyhow::Error::new(HandshakeError { source: error });
    let delay = retry_delay(error.as_ref(), 250);
    assert_eq!(delay, Duration::from_secs(2));
    let start = time::Instant::now();
    assert!(sleep_with_retry_deadline(delay, None).await);
    assert_eq!(start.elapsed(), delay);
    let start = time::Instant::now();
    assert!(!sleep_with_retry_deadline(delay, Some(Instant::now() + Duration::from_millis(100))).await);
    assert!(start.elapsed() <= Duration::from_millis(101));
}

#[test]
fn retryable_initialize_error_includes_initialized_notification_context() {
    let contexts = [
        "send initialize request",
        "send initialized notification",
        "receive initialize response",
    ];

    assert_eq!(
        contexts.map(|context| {
            RmcpClient::is_retryable_client_initialize_error(&retryable_initialize_error(context))
        }),
        [true, true, false],
    );
}

#[test]
fn retryable_streamable_http_error_includes_remote_body_stream_failure() {
    let errors = [
        StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
            ExecServerError::HttpRequest("error sending request for url".to_string()),
        )),
        StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
            ExecServerError::Server {
                code: JSON_RPC_INTERNAL_ERROR_CODE,
                message: "http/request failed: error sending request for url".to_string(),
            },
        )),
        StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
            ExecServerError::Protocol(
                "http response stream `http-1` failed: exec-server transport disconnected"
                    .to_string(),
            ),
        )),
        StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
            ExecServerError::Protocol(
                "http response stream `http-1` received seq 2, expected 1".to_string(),
            ),
        )),
        StreamableHttpError::Client(StreamableHttpClientAdapterError::UnexpectedHttpStatus {
            status: StatusCode::BAD_GATEWAY,
            body_preview: "localized upstream failure".to_string(),
            retry_after: None,
        }),
        StreamableHttpError::Client(StreamableHttpClientAdapterError::UnexpectedHttpStatus {
            status: StatusCode::BAD_REQUEST,
            body_preview: "localized bad request".to_string(),
            retry_after: None,
        }),
    ];

    assert_eq!(
        errors.map(|error| RmcpClient::is_retryable_streamable_http_error(&error)),
        [true, true, true, false, true, false],
    );
}

fn retryable_initialize_error(context: &'static str) -> rmcp::service::ClientInitializeError {
    rmcp::service::ClientInitializeError::TransportError {
        error: DynamicTransportError::from_parts(
            "streamable_http",
            TypeId::of::<()>(),
            Box::new(StreamableHttpError::Client(
                StreamableHttpClientAdapterError::HttpRequest(ExecServerError::HttpRequest(
                    "error sending request for url".to_string(),
                )),
            )),
        ),
        context: context.into(),
    }
}
