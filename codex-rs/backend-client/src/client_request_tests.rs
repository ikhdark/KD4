use std::sync::Arc;

use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn client_preserves_supplied_http_client_factory_policy() {
    let client = Client::new(
        "https://example.test",
        HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
    );

    assert_eq!(
        client.http.outbound_proxy_policy(),
        OutboundProxyPolicy::RespectSystemProxy
    );
}

#[test]
fn list_tasks_url_omits_empty_query_and_encodes_all_parameters() {
    let client = Client::new(
        "https://example.test",
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );

    assert_eq!(
        client
            .list_tasks_url(
                /*limit*/ None, /*task_filter*/ None, /*environment_id*/ None,
                /*cursor*/ None,
            )
            .unwrap(),
        "https://example.test/api/codex/tasks/list"
    );
    assert_eq!(
        client
            .list_tasks_url(
                /*limit*/ Some(10),
                /*task_filter*/ Some("mine / shared"),
                /*environment_id*/ Some("env&one"),
                /*cursor*/ Some("next=page"),
            )
            .unwrap(),
        "https://example.test/api/codex/tasks/list?limit=10&task_filter=mine+%2F+shared&cursor=next%3Dpage&environment_id=env%26one"
    );
}

#[tokio::test]
async fn migrated_requests_preserve_query_auth_and_json_body() {
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/tasks/list"))
        .and(query_param("limit", "10"))
        .and(query_param("task_filter", "mine / shared"))
        .and(query_param("environment_id", "env&one"))
        .and(query_param("cursor", "next=page"))
        .and(header("authorization", "Bearer request-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": []})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/codex/tasks"))
        .and(header("authorization", "Bearer request-token"))
        .and(body_json(serde_json::json!({"prompt": "hello"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"task": {"id": "task-created"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(
        server.uri(),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .with_auth_provider(Arc::new(codex_model_provider::BearerAuthProvider::new(
        "request-token".to_string(),
    )));
    let tasks = client
        .list_tasks(
            Some(10),
            Some("mine / shared"),
            Some("env&one"),
            Some("next=page"),
        )
        .await
        .expect("list request should succeed");
    let task_id = client
        .create_task(serde_json::json!({"prompt": "hello"}))
        .await
        .expect("create request should succeed");
    assert_eq!(tasks, PaginatedListTaskListItem::new(Vec::new()));
    assert_eq!(task_id, "task-created");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    server.verify().await;
}

#[test]
fn base_urls_use_exact_host_and_path_components() {
    for (base, expected) in [
        (
            "https://chatgpt.com.example.test",
            "https://chatgpt.com.example.test/api/codex/tasks/list",
        ),
        (
            "https://CHATGPT.COM/",
            "https://chatgpt.com/backend-api/wham/tasks/list",
        ),
        (
            "https://example.test?next=/backend-api",
            "https://example.test/api/codex/tasks/list?next=/backend-api",
        ),
        (
            "https://example.test/backend-api-other/",
            "https://example.test/backend-api-other/api/codex/tasks/list",
        ),
        (
            "https://example.test/backend-api/",
            "https://example.test/backend-api/wham/tasks/list",
        ),
    ] {
        let client = Client::new(
            base,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        );
        assert_eq!(
            client.list_tasks_url(None, None, None, None).unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn task_requests_encode_ids_as_single_path_segments() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    for prefix in ["/api/codex", "/backend-api/wham"] {
        for suffix in ["", "/turns/turn%2F%3F%23%25/sibling_turns"] {
            Mock::given(method("GET"))
                .and(path(format!("{prefix}/tasks/task%2F%3F%23%25{suffix}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"sibling_turns": []})),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
    }
    for suffix in ["", "/backend-api"] {
        let client = Client::new(
            format!("{}{suffix}", server.uri()),
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        );
        let details = client.get_task_details("task/?#%").await.unwrap();
        assert!(details.current_assistant_turn.is_none());
        assert!(
            client
                .list_sibling_turns("task/?#%", "turn/?#%")
                .await
                .unwrap()
                .sibling_turns
                .is_empty()
        );
        assert!(client.get_task_details("..").await.is_err());
        assert!(client.list_sibling_turns("task", ".").await.is_err());
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    server.verify().await;
}

#[tokio::test]
async fn error_responses_and_decode_diagnostics_are_bounded() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let large_body = "x".repeat(MAX_DIAGNOSTIC_BODY_BYTES * 4);
    Mock::given(method("GET"))
        .and(path("/api/codex/settings/user"))
        .respond_with(ResponseTemplate::new(401).set_body_string(large_body.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/tasks/list"))
        .respond_with(ResponseTemplate::new(200).set_body_string(large_body))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(
        server.uri(),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let error = client.get_user_settings().await.unwrap_err();
    assert!(error.is_unauthorized());
    let RequestError::UnexpectedStatus { body, .. } = error else {
        panic!("expected status error")
    };
    assert_eq!(
        body,
        format!("{} [truncated]", "x".repeat(MAX_DIAGNOSTIC_BODY_BYTES))
    );
    let error = client
        .list_tasks(None, None, None, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Decode error"));
    assert!(error.ends_with(" [truncated]"));
    assert!(error.len() < MAX_DIAGNOSTIC_BODY_BYTES + 512);
    server.verify().await;
}

#[tokio::test]
async fn broken_response_bodies_preserve_status_and_fail_successful_requests() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    // Malformed framing requires a raw peer; accepting, reading, and joining are bounded.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for status in ["401 Unauthorized", "200 OK", "200 OK"] {
            let (mut stream, _) = timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            timeout(Duration::from_secs(5), async {
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                loop {
                    let size = stream.read(&mut buf).await.unwrap();
                    assert!(size > 0, "client closed before sending request headers");
                    request.extend_from_slice(&buf[..size]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") { break; }
                }
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort").as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }).await.unwrap();
        }
    });
    let client = Client::new(
        format!("http://{address}"),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let error = client.get_user_settings().await.unwrap_err();
    assert!(error.is_unauthorized());
    let RequestError::UnexpectedStatus { body, .. } = error else {
        panic!("expected HTTP status")
    };
    assert!(body.contains("body read failed:"));
    let error = client
        .send_add_credits_nudge_email(AddCreditsNudgeCreditType::Credits)
        .await
        .unwrap_err();
    assert!(matches!(error, RequestError::Other(_)));
    assert!(error.to_string().contains("error decoding response body"));
    let error = client.list_tasks(None, None, None, None).await.unwrap_err();
    assert!(error.to_string().contains("error decoding response body"));
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}
