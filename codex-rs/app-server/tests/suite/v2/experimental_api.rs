use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use app_test_support::to_response;
use app_test_support::write_mock_provider_config_toml;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::MockExperimentalMethodParams;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Experimental methods and fields are rejected at the capability gate without
/// disturbing the connection, so one stable client proves every surface and
/// then starts a thread without experimental input.
#[tokio::test]
async fn experimental_api_surfaces_require_capability() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(Vec::new()).await;
    let codex_home = TempDir::new()?;
    write_mock_provider_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let init = mcp
        .initialize_with_capabilities(
            default_client_info(),
            Some(InitializeCapabilities {
                experimental_api: false,
                request_attestation: false,
                opt_out_notification_methods: None,
                mcp_server_openai_form_elicitation: false,
            }),
        )
        .await?;
    let JSONRPCMessage::Response(_) = init else {
        anyhow::bail!("expected initialize response, got {init:?}");
    };

    let request_id = mcp
        .send_mock_experimental_method_request(MockExperimentalMethodParams::default())
        .await?;
    expect_experimental_capability_error(&mut mcp, request_id, "mock/experimentalMethod").await?;

    let request_id = mcp
        .send_thread_settings_update_request(ThreadSettingsUpdateParams {
            thread_id: "thr_123".to_string(),
            ..Default::default()
        })
        .await?;
    expect_experimental_capability_error(&mut mcp, request_id, "thread/settings/update").await?;

    let request_id = mcp
        .send_thread_start_request(ThreadStartParams {
            mock_experimental_field: Some("mock".to_string()),
            ..Default::default()
        })
        .await?;
    expect_experimental_capability_error(
        &mut mcp,
        request_id,
        "thread/start.mockExperimentalField",
    )
    .await?;

    let request_id = mcp
        .send_thread_start_request(ThreadStartParams {
            approval_policy: Some(AskForApproval::Granular {
                sandbox_approval: true,
                rules: false,
                skill_approval: false,
                request_permissions: true,
                mcp_elicitations: false,
            }),
            ..Default::default()
        })
        .await?;
    expect_experimental_capability_error(&mut mcp, request_id, "askForApproval.granular").await?;

    let request_id = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let _: ThreadStartResponse = to_response(response)?;
    Ok(())
}

fn default_client_info() -> ClientInfo {
    ClientInfo {
        name: DEFAULT_CLIENT_NAME.to_string(),
        title: None,
        version: "0.1.0".to_string(),
    }
}

async fn expect_experimental_capability_error(
    mcp: &mut TestAppServer,
    request_id: i64,
    reason: &str,
) -> Result<()> {
    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    assert_eq!(
        error.error.message,
        format!("{reason} requires experimentalApi capability")
    );
    assert_eq!(error.error.data, None);
    Ok(())
}
