use super::*;
use core_test_support::require_network;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[test]
fn device_code_prompt_renders_phishing_warning() {
    let prompt = device_code_prompt("https://example.com/device", "ABCD-EFGH");

    assert!(prompt.contains(
        "\x1b[90mContinue only if you started this login in Codex. If a website or another person gave you this code, cancel.\x1b[0m"
    ));
}

#[tokio::test]
async fn token_poll_deadline_bounds_a_delayed_success_response() -> anyhow::Result<()> {
    require_network!();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/deviceauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(serde_json::json!({
                    "authorization_code": "expired-code",
                    "code_challenge": "challenge",
                    "code_verifier": "verifier"
                })),
        )
        .mount(&server)
        .await;
    let client = create_raw_auth_client_async(
        &server.uri(),
        &crate::test_support::transport_default_auth_route_config(),
    )
    .await?;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        poll_for_token_with_timeout(
            &client,
            &server.uri(),
            "device-id",
            "user-code",
            0,
            Duration::from_millis(50),
        ),
    )
    .await?;
    assert!(
        matches!(result, Err(ref error) if error.kind() == io::ErrorKind::TimedOut),
        "the polling budget must include the HTTP response wait"
    );
    Ok(())
}

#[tokio::test]
async fn token_poll_does_not_send_another_request_after_expiry() -> anyhow::Result<()> {
    require_network!();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/deviceauth/token"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let client = create_raw_auth_client_async(
        &server.uri(),
        &crate::test_support::transport_default_auth_route_config(),
    )
    .await?;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        poll_for_token_with_timeout(
            &client,
            &server.uri(),
            "device-id",
            "user-code",
            1,
            Duration::from_millis(50),
        ),
    )
    .await?;
    let requests = server.received_requests().await.expect("request recording");
    assert!(requests.len() <= 1, "expiry must not initiate another poll");
    assert!(
        matches!(result, Err(ref error) if error.kind() == io::ErrorKind::TimedOut),
        "an expired polling budget must report timeout"
    );
    Ok(())
}
