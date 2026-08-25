use anyhow::Result;
use anyhow::bail;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::DesktopActivationUnavailableReason;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadDesktopActivationChallengeParams;
use codex_app_server_protocol::ThreadDesktopActivationChallengeResponse;
use codex_app_server_protocol::ThreadDesktopActivationObligationParams;
use codex_app_server_protocol::ThreadDesktopActivationObligationResponse;
use codex_app_server_protocol::ThreadDesktopActivationRecordParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use pretty_assertions::assert_eq;
use serde_json::to_value;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test]
async fn desktop_activation_methods_are_wired_through_initialized_connection() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let initialized = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_capabilities(
            ClientInfo {
                name: "codex_desktop".to_string(),
                title: Some("Codex Desktop".to_string()),
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                desktop_activation_receipts: true,
                ..Default::default()
            }),
        ),
    )
    .await??;
    let JSONRPCMessage::Response(_) = initialized else {
        bail!("expected initialize response, got {initialized:?}");
    };

    let thread_request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let thread_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_request_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(thread_response)?;

    let obligation_request_id = mcp
        .send_raw_request(
            "thread/desktopActivation/obligation",
            Some(to_value(ThreadDesktopActivationObligationParams {
                thread_id: thread.id.clone(),
            })?),
        )
        .await?;
    let obligation_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(obligation_request_id)),
    )
    .await??;
    let obligation: ThreadDesktopActivationObligationResponse = to_response(obligation_response)?;
    assert_eq!(obligation.obligation, None);

    let challenge_request_id = mcp
        .send_raw_request(
            "thread/desktopActivation/challenge",
            Some(to_value(ThreadDesktopActivationChallengeParams {
                thread_id: thread.id,
            })?),
        )
        .await?;
    let challenge_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(challenge_request_id)),
    )
    .await??;
    let challenge: ThreadDesktopActivationChallengeResponse = to_response(challenge_response)?;
    assert_eq!(challenge.challenge, None);
    assert_eq!(
        challenge.unavailable_reason,
        Some(DesktopActivationUnavailableReason::NoCurrentActivationObligation)
    );

    let record_request_id = mcp
        .send_raw_request(
            "thread/desktopActivation/record",
            Some(to_value(ThreadDesktopActivationRecordParams {
                challenge_id: "unknown-challenge".to_string(),
                desktop_process_id: 1,
                desktop_executable_path: "C:\\Codex\\Codex.exe".to_string(),
                observation_timestamp: "2026-01-01T00:00:00Z".to_string(),
                initialization_observation_identity: "initialization-observation".to_string(),
            })?),
        )
        .await?;
    let record_error = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(record_request_id)),
    )
    .await??;
    assert_eq!(record_error.error.code, -32600);
    assert_eq!(
        record_error.error.message,
        "unknown Desktop activation challenge"
    );

    Ok(())
}

fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}
