//! Real HTTP/concurrent refresh coverage with an instance-local expiry fixture.
use super::*;
use crate::oauth::WrappedOAuthTokenResponse;
use crate::oauth::save_oauth_tokens;
use codex_exec_server::Environment;
use oauth2::AccessToken;
use oauth2::RefreshToken;
use oauth2::basic::BasicTokenType;
use rmcp::transport::auth::OAuthTokenResponse;
use rmcp::transport::auth::VendorExtraTokenFields;
use serde_json::Value;
use serde_json::json;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::method;
use wiremock::matchers::path;

const SERVER_NAME: &str = "test-streamable-http-oauth-refresh";
const REFRESH_TOKEN: &str = "valid-refresh-token";
const REFRESHED_ACCESS_TOKEN: &str = "refreshed-access-token";

async fn oauth_client_fixture()
-> anyhow::Result<(MockServer, TempDir, Arc<RmcpClient>, OAuthPersistor)> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_endpoint": format!("{}/oauth/authorize", server.uri()),
            "token_endpoint": format!("{}/oauth/token", server.uri()),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": REFRESHED_ACCESS_TOKEN,
            "token_type": "Bearer",
            "expires_in": 7200,
            "refresh_token": REFRESH_TOKEN,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(|request: &Request| {
            let body: Value = request.body_json().expect("valid JSON-RPC request");
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            match body.get("method").and_then(Value::as_str) {
                Some("initialize") => ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": body
                            .pointer("/params/protocolVersion")
                            .cloned()
                            .unwrap_or_else(|| json!("2025-06-18")),
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "oauth-refresh-test", "version": "0.0.0-test" },
                    },
                })),
                Some("notifications/initialized") => ResponseTemplate::new(202),
                Some("tools/list") => ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": [] },
                })),
                method => ResponseTemplate::new(400)
                    .set_body_string(format!("unexpected JSON-RPC method: {method:?}")),
            }
        })
        .mount(&server)
        .await;

    // Initialize with a comfortably valid token, then move only this client's
    // in-memory credentials across RMCP's 30-second proactive refresh boundary.
    let codex_home = TempDir::new()?;
    let server_url = format!("{}/mcp", server.uri());
    let now_ms = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    let mut response = OAuthTokenResponse::new(
        AccessToken::new("near-expiry-access-token".to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    response.set_refresh_token(Some(RefreshToken::new(REFRESH_TOKEN.to_string())));
    save_oauth_tokens(
        codex_home.path(),
        SERVER_NAME,
        &StoredOAuthTokens {
            server_name: SERVER_NAME.to_string(),
            url: server_url.clone(),
            client_id: "test-client-id".to_string(),
            token_response: WrappedOAuthTokenResponse(response),
            expires_at: Some(now_ms + 3_600_000),
        },
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let client = RmcpClient::new_streamable_http_client(
        SERVER_NAME,
        codex_home.path().to_path_buf(),
        &server_url,
        /*bearer_token*/ None,
        /*http_headers*/ None,
        /*env_http_headers*/ None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        Environment::default_for_tests().get_http_client(),
        /*auth_provider*/ None,
    )
    .await?;
    client
        .initialize(
            rmcp::model::InitializeRequestParams::new(
                Default::default(),
                rmcp::model::Implementation::new("codex-test", "0.0.0-test"),
            )
            .with_protocol_version(rmcp::model::ProtocolVersion::V_2025_06_18),
            Some(Duration::from_secs(5)),
            Box::new(|_, _| async { unreachable!("no elicitation in this fixture") }.boxed()),
            Box::new(|_| async {}.boxed()),
        )
        .await?;
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/oauth/token"),
        "initialization must not spend the refresh being tested"
    );

    let oauth = {
        let state = client.state.lock().await;
        let ClientState::Ready {
            oauth: Some(oauth), ..
        } = &*state
        else {
            panic!("initialized client must retain its OAuth runtime");
        };
        oauth.clone()
    };
    Ok((server, codex_home, Arc::new(client), oauth))
}

/// Concurrent operations inside the refresh window share one token-endpoint call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_operations_near_expiry_share_one_token_refresh() -> anyhow::Result<()> {
    let (server, _home, client, oauth) = oauth_client_fixture().await?;
    oauth
        .set_remaining_lifetime_for_test(Duration::from_secs(29))
        .await?;
    let (first, second) = tokio::join!(
        client.list_tools(/*params*/ None, Some(Duration::from_secs(5))),
        client.list_tools(/*params*/ None, Some(Duration::from_secs(5))),
    );
    first?;
    second?;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/oauth/token")
            .count(),
        1
    );
    let tool_requests: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.url.path() == "/mcp"
                && request.method == "POST"
                && request.body_json::<Value>().unwrap()["method"] == "tools/list"
        })
        .collect();
    assert_eq!(tool_requests.len(), 2);
    let expected_authorization = format!("Bearer {REFRESHED_ACCESS_TOKEN}");
    for request in tool_requests {
        assert_eq!(
            request.headers.get("authorization").unwrap().to_str()?,
            expected_authorization
        );
    }
    server.verify().await;
    Ok(())
}

/// A tools/list that already reached the server must not join another request's
/// refresh merely to sample credentials for persistence. Exercise the real
/// transport, result delivery and file store, not just the mutex helper.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_operation_does_not_wait_for_another_refresh() -> anyhow::Result<()> {
    let (server, home, client, oauth) = oauth_client_fixture().await?;
    let first_received = Arc::new(tokio::sync::Notify::new());
    let refresh_received = Arc::new(tokio::sync::Notify::new());
    let first_signal = Arc::clone(&first_received);
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("tools/list"))
        .respond_with(move |request: &Request| {
            first_signal.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": request.body_json::<Value>().unwrap()["id"],
                    "result": { "tools": [] },
                }))
                .set_delay(Duration::from_millis(100))
        })
        .with_priority(1)
        .expect(2)
        .mount(&server)
        .await;
    let refresh_signal = Arc::clone(&refresh_received);
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(move |_: &Request| {
            refresh_signal.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "access_token": REFRESHED_ACCESS_TOKEN, "token_type": "Bearer",
                    "expires_in": 7200, "refresh_token": REFRESH_TOKEN,
                }))
                .set_delay(Duration::from_millis(1500))
        })
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    let first = async {
        let result = client.list_tools(None, Some(Duration::from_secs(5))).await;
        let elapsed = started.elapsed();
        let refresh_started = refresh_received.notified().now_or_never().is_some();
        (result, elapsed, refresh_started)
    };
    let second = async {
        tokio::time::timeout(Duration::from_secs(5), first_received.notified()).await?;
        oauth
            .set_remaining_lifetime_for_test(Duration::from_secs(29))
            .await?;
        client.list_tools(None, Some(Duration::from_secs(5))).await
    };
    let ((first_result, first_elapsed, refresh_started), second_result) =
        tokio::join!(first, second);
    first_result?;
    second_result?;
    assert!(
        refresh_started,
        "the fixture must overlap the completed call with a refresh"
    );
    let persisted = crate::load_oauth_tokens(
        home.path(),
        SERVER_NAME,
        &format!("{}/mcp", server.uri()),
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .expect("refreshed credentials must be durable");
    assert_eq!(
        persisted.token_response.0.access_token().secret(),
        REFRESHED_ACCESS_TOKEN
    );
    server.verify().await;
    eprintln!(
        "completed MCP operation: first={first_elapsed:?}; refresh_delay=1500ms; requests=2; refreshes=1"
    );
    assert!(
        first_elapsed < Duration::from_millis(800),
        "a completed operation waited for unrelated authentication: {first_elapsed:?}"
    );
    Ok(())
}
