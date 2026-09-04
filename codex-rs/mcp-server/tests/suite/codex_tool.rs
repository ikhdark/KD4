use std::env;
use std::path::Path;

use codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR;
use codex_mcp_server::CodexToolCallParam;
use codex_mcp_server::ExecApprovalElicitRequestParams;
use codex_mcp_server::ExecApprovalResponse;
use codex_protocol::protocol::ReviewDecision;
use codex_shell_command::parse_command;
use pretty_assertions::assert_eq;
use rmcp::model::JsonRpcResponse;
use rmcp::model::JsonRpcVersion2_0;
use rmcp::model::RequestId;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

use core_test_support::BlockedCompletionProofFixture;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use mcp_test_support::McpProcess;
use mcp_test_support::create_final_assistant_message_sse_response;
use mcp_test_support::create_mock_responses_server;
use mcp_test_support::create_shell_command_sse_response;
use mcp_test_support::format_with_current_shell;

// Windows CI can spend tens of seconds in session startup before the first
// mock model request is sent.
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_failure_shuts_down_with_stdin_still_open() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp_process = McpProcess::new(codex_home.path()).await?;
    mcp_process.close_stdout();
    mcp_process.send_ping_request().await?;

    let status = timeout(DEFAULT_READ_TIMEOUT, mcp_process.wait_for_exit()).await??;
    assert!(status.success(), "MCP server exited with {status}");
    Ok(())
}

/// Test that a shell command that is not on the "trusted" list triggers an
/// elicitation request to the MCP and that sending the approval runs the
/// command, as expected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_shell_command_approval_triggers_elicitation() {
    if env::var(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
        println!(
            "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
        );
        return;
    }

    // Apparently `#[tokio::test]` must return `()`, so we create a helper
    // function that returns `Result` so we can use `?` in favor of `unwrap`.
    shell_command_approval_triggers_elicitation()
        .await
        .expect("shell command approval should trigger elicitation");
}

async fn shell_command_approval_triggers_elicitation() -> anyhow::Result<()> {
    // Use a simple, untrusted command that creates a file so we can
    // observe a side-effect.
    let workdir_for_shell_function_call = TempDir::new()?;
    let created_filename = "created_by_shell_tool.txt";
    let created_file = workdir_for_shell_function_call
        .path()
        .join(created_filename);

    let (shell_command, timeout_ms) = (
        vec![
            "New-Item".to_string(),
            "-ItemType".to_string(),
            "File".to_string(),
            "-Path".to_string(),
            created_filename.to_string(),
            "-Force".to_string(),
        ],
        // `powershell.exe` startup can be slow on loaded Windows CI workers
        10_000,
    );
    let expected_shell_command =
        format_with_current_shell(&shlex::try_join(shell_command.iter().map(String::as_str))?);

    let McpHandle {
        process: mut mcp_process,
        server: _server,
        dir: _dir,
    } = create_mcp_process(vec![
        create_shell_command_sse_response(
            shell_command.clone(),
            Some(workdir_for_shell_function_call.path()),
            Some(timeout_ms),
            "call1234",
        )?,
        create_final_assistant_message_sse_response("File created!")?,
    ])
    .await?;

    // Send a "codex" tool request, which should hit the responses endpoint.
    // In turn, it should reply with a tool call, which the MCP should forward
    // as an elicitation.
    let codex_request_id = mcp_process
        .send_codex_tool_call(CodexToolCallParam {
            prompt: "run `git init`".to_string(),
            cwd: Some(
                workdir_for_shell_function_call
                    .path()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        })
        .await?;
    let elicitation_request = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_request_message(),
    )
    .await??;

    assert_eq!(elicitation_request.jsonrpc, JsonRpcVersion2_0);
    assert_eq!(elicitation_request.request.method, "elicitation/create");

    let elicitation_request_id = elicitation_request.id.clone();
    let params = serde_json::from_value::<ExecApprovalElicitRequestParams>(
        elicitation_request
            .request
            .params
            .clone()
            .ok_or_else(|| anyhow::anyhow!("elicitation_request.params must be set"))?,
    )?;
    assert_eq!(
        elicitation_request.request.params,
        Some(create_expected_elicitation_request_params(
            expected_shell_command,
            workdir_for_shell_function_call.path(),
            codex_request_id.to_string(),
            params.codex_event_id.clone(),
            params.thread_id,
        )?)
    );

    // Accept the `git init` request by responding to the elicitation.
    mcp_process
        .send_response(
            elicitation_request_id,
            serde_json::to_value(ExecApprovalResponse {
                decision: ReviewDecision::Approved,
            })?,
        )
        .await?;

    // Verify task_complete notification arrives before the tool call completes.
    let _task_complete = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_legacy_task_complete_notification(),
    )
    .await
    .expect("task_complete_notification timeout")
    .expect("task_complete_notification resp");

    // Verify the original `codex` tool call completes and that the file was created.
    let codex_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_response_message(RequestId::Number(codex_request_id)),
    )
    .await??;
    assert_eq!(
        JsonRpcResponse {
            jsonrpc: JsonRpcVersion2_0,
            id: RequestId::Number(codex_request_id),
            result: json!({
                "content": [
                    {
                        "text": "File created!",
                        "type": "text"
                    }
                ],
                "structuredContent": {
                    "threadId": params.thread_id,
                    "content": "File created!"
                }
            }),
        },
        codex_response
    );

    assert!(created_file.is_file(), "created file should exist");

    Ok(())
}

fn create_expected_elicitation_request_params(
    command: Vec<String>,
    workdir: &Path,
    codex_mcp_tool_call_id: String,
    codex_event_id: String,
    thread_id: codex_protocol::ThreadId,
) -> anyhow::Result<serde_json::Value> {
    let expected_message = format!(
        "Allow Codex to run `{}` in `{}`?",
        shlex::try_join(command.iter().map(std::convert::AsRef::as_ref))?,
        workdir.to_string_lossy()
    );
    let codex_parsed_cmd = parse_command::parse_command(&command);
    let params_json = serde_json::to_value(ExecApprovalElicitRequestParams {
        message: expected_message,
        requested_schema: json!({"type":"object","properties":{}}),
        thread_id,
        codex_elicitation: "exec-approval".to_string(),
        codex_mcp_tool_call_id,
        codex_event_id,
        codex_command: command,
        codex_cwd: workdir.to_path_buf(),
        codex_call_id: "call1234".to_string(),
        codex_parsed_cmd,
    })?;
    Ok(params_json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_codex_tool_passes_base_instructions() {
    skip_if_no_network!();

    // Apparently `#[tokio::test]` must return `()`, so we create a helper
    // function that returns `Result` so we can use `?` in favor of `unwrap`.
    codex_tool_passes_base_instructions()
        .await
        .expect("codex tool should pass base instructions");
}

async fn codex_tool_passes_base_instructions() -> anyhow::Result<()> {
    #![expect(clippy::unwrap_used)]

    let server =
        create_mock_responses_server(vec![create_final_assistant_message_sse_response("Enjoy!")?])
            .await;

    // Run `codex mcp` with a specific config.toml.
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut mcp_process = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp_process.initialize()).await??;

    // Send a "codex" tool request, which should hit the responses endpoint.
    let codex_request_id = mcp_process
        .send_codex_tool_call(CodexToolCallParam {
            prompt: "How are you?".to_string(),
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            base_instructions: Some("You are a helpful assistant.".to_string()),
            developer_instructions: Some("Foreshadow upcoming tool calls.".to_string()),
            ..Default::default()
        })
        .await?;

    let codex_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_response_message(RequestId::Number(codex_request_id)),
    )
    .await??;
    assert_eq!(codex_response.jsonrpc, JsonRpcVersion2_0);
    assert_eq!(codex_response.id, RequestId::Number(codex_request_id));
    assert_eq!(
        codex_response.result,
        json!({
            "content": [
                {
                    "text": "Enjoy!",
                    "type": "text"
                }
            ],
            "structuredContent": {
                "threadId": codex_response
                    .result
                    .get("structuredContent")
                    .and_then(|v| v.get("threadId"))
                    .and_then(serde_json::Value::as_str)
                    .expect("codex tool response should include structuredContent.threadId"),
                "content": "Enjoy!"
            }
        })
    );

    let requests = server.received_requests().await.unwrap();
    let request = requests[0].body_json::<serde_json::Value>()?;
    let instructions = request["instructions"]
        .as_str()
        .expect("responses request should include instructions");
    assert!(instructions.starts_with("You are a helpful assistant."));

    let developer_messages: Vec<&serde_json::Value> = request["input"]
        .as_array()
        .expect("responses request should include input items")
        .iter()
        .filter(|msg| msg.get("role").and_then(|role| role.as_str()) == Some("developer"))
        .collect();
    let developer_contents: Vec<&str> = developer_messages
        .iter()
        .filter_map(|msg| msg.get("content").and_then(serde_json::Value::as_array))
        .flat_map(|content| content.iter())
        .filter(|span| span.get("type").and_then(serde_json::Value::as_str) == Some("input_text"))
        .filter_map(|span| span.get("text").and_then(serde_json::Value::as_str))
        .collect();
    assert!(
        developer_contents
            .iter()
            .any(|content| content.contains("`sandbox_mode`")),
        "expected permissions developer message, got {developer_contents:?}"
    );
    assert!(
        developer_contents.contains(&"Foreshadow upcoming tool calls."),
        "expected developer instructions in developer messages, got {developer_contents:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_completion_proof_reaches_mcp_as_error_without_launching_runner()
-> anyhow::Result<()> {
    let proof_protocol_timeout = std::time::Duration::from_secs(120);

    let fixture = BlockedCompletionProofFixture::new()?;
    let responses = (0..33)
        .map(|_| create_final_assistant_message_sse_response("premature success"))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let McpHandle {
        process: mut mcp_process,
        server: _server,
        dir: codex_home,
    } = create_mcp_process(responses).await?;
    let physical_credential_before =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            codex_home.path(),
            fixture.repo_path(),
        )
        .map_err(anyhow::Error::msg)?;

    let request_id = mcp_process
        .send_codex_tool_call(CodexToolCallParam {
            prompt: "finish without running certification".to_string(),
            cwd: Some(fixture.repo_path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let (response, observed_payloads) = timeout(
        proof_protocol_timeout,
        mcp_process.read_stream_until_response_message_with_observed_payloads(RequestId::Number(
            request_id,
        )),
    )
    .await??;

    assert!(
        observed_payloads
            .iter()
            .all(|payload| !payload.contains("premature success")),
        "MCP published the buffered assistant success: {observed_payloads:#?}"
    );
    assert_eq!(response.result.get("isError"), Some(&json!(true)));
    let error_text = response
        .result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("MCP error response should include text content"))?;
    assert!(
        error_text.contains("CompletionProofGate blocked terminal success"),
        "unexpected MCP terminal error: {error_text}"
    );
    assert!(
        !fixture.canonical_runner_launched(),
        "MCP must not launch canonical certification"
    );
    timeout(proof_protocol_timeout, mcp_process.hard_kill_and_wait()).await??;
    let physical_credential_after =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            codex_home.path(),
            fixture.repo_path(),
        )
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        physical_credential_after, physical_credential_before,
        "hard-killed MCP server changed the exact physical completion-proof credential"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logical_generation_limit_reaches_mcp_as_failed_turn_complete() -> anyhow::Result<()> {
    const WORK_LIMIT_ERROR: &str =
        "The turn reached its logical generation limit before all requested work completed.";
    let work_limit_protocol_timeout = std::time::Duration::from_secs(120);

    let responses = (0..33)
        .map(create_update_plan_sse_response)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let workdir = TempDir::new()?;
    let git_init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(workdir.path())
        .status()?;
    anyhow::ensure!(git_init.success(), "git init failed for MCP fixture");
    let McpHandle {
        process: mut mcp_process,
        server,
        dir: _dir,
    } = create_mcp_process(responses).await?;

    let request_id = mcp_process
        .send_codex_tool_call(CodexToolCallParam {
            prompt: "keep updating the plan forever".to_string(),
            cwd: Some(workdir.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let (response, observed_payloads) = timeout(
        work_limit_protocol_timeout,
        mcp_process.read_stream_until_response_message_with_observed_payloads(RequestId::Number(
            request_id,
        )),
    )
    .await??;
    let observed_messages = observed_payloads
        .iter()
        .map(|payload| serde_json::from_str::<serde_json::Value>(payload))
        .collect::<serde_json::Result<Vec<_>>>()?;

    let error_index = observed_messages
        .iter()
        .position(|message| {
            message.get("method").and_then(serde_json::Value::as_str) == Some("codex/event")
                && message
                    .pointer("/params/msg/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("error")
                && message
                    .pointer("/params/msg/message")
                    .and_then(serde_json::Value::as_str)
                    == Some(WORK_LIMIT_ERROR)
        })
        .ok_or_else(|| anyhow::anyhow!("missing status-affecting work-limit error notification"))?;
    let (turn_complete_index, turn_complete) = observed_messages
        .iter()
        .enumerate()
        .find(|(_, message)| {
            message.get("method").and_then(serde_json::Value::as_str) == Some("codex/event")
                && message
                    .pointer("/params/msg/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("task_complete")
        })
        .ok_or_else(|| anyhow::anyhow!("missing failed TurnComplete notification"))?;

    assert!(
        error_index < turn_complete_index,
        "work-limit error must precede failed TurnComplete: {observed_payloads:#?}"
    );
    assert!(
        turn_complete_index + 1 < observed_messages.len(),
        "failed TurnComplete must precede the tools/call response: {observed_payloads:#?}"
    );
    assert_eq!(
        turn_complete
            .pointer("/params/msg/error/message")
            .and_then(serde_json::Value::as_str),
        Some(WORK_LIMIT_ERROR)
    );
    assert!(
        turn_complete
            .pointer("/params/msg/last_agent_message")
            .is_none()
            || turn_complete
                .pointer("/params/msg/last_agent_message")
                .is_some_and(serde_json::Value::is_null),
        "failed TurnComplete exposed success text: {turn_complete}"
    );
    assert!(
        turn_complete
            .pointer("/params/msg/surfaced_result")
            .is_none()
            || turn_complete
                .pointer("/params/msg/surfaced_result")
                .is_some_and(serde_json::Value::is_null),
        "failed TurnComplete exposed success metadata: {turn_complete}"
    );

    assert_eq!(response.result.get("isError"), Some(&json!(true)));
    assert_eq!(
        response
            .result
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str),
        Some(WORK_LIMIT_ERROR)
    );
    assert_eq!(
        response
            .result
            .pointer("/structuredContent/content")
            .and_then(serde_json::Value::as_str),
        Some(WORK_LIMIT_ERROR)
    );
    assert!(
        response
            .result
            .pointer("/structuredContent/surfacedResult")
            .is_none()
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("mock server did not record response requests"))?
            .len(),
        33,
        "work-limit path should use 32 regular generations and one terminal generation"
    );

    Ok(())
}

fn create_update_plan_sse_response(generation: usize) -> anyhow::Result<String> {
    let response_id = format!("resp-plan-{generation}");
    let call_id = format!("call-plan-{generation}");
    let arguments = serde_json::to_string(&json!({
        "plan": [{
            "step": format!("continue generation {generation}"),
            "status": "in_progress",
        }],
    }))?;
    Ok(responses::sse(vec![
        responses::ev_response_created(&response_id),
        responses::ev_function_call(&call_id, "update_plan", &arguments),
        responses::ev_completed(&response_id),
    ]))
}

/// This handle is used to ensure that the MockServer and TempDir are not dropped while
/// the McpProcess is still running.
pub struct McpHandle {
    pub process: McpProcess,
    /// Retain the server for the lifetime of the McpProcess.
    #[allow(dead_code)]
    server: MockServer,
    /// Retain the temporary directory for the lifetime of the McpProcess.
    #[allow(dead_code)]
    dir: TempDir,
}

async fn create_mcp_process(responses: Vec<String>) -> anyhow::Result<McpHandle> {
    let server = create_mock_responses_server(responses).await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut mcp_process = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp_process.initialize()).await??;
    Ok(McpHandle {
        process: mcp_process,
        server,
        dir: codex_home,
    })
}

/// Create a Codex config that uses the mock server as the model provider.
/// It also uses `approval_policy = "untrusted"` so that we exercise the
/// elicitation code path for shell commands.
fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "untrusted"
sandbox_policy = "workspace-write"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0

[features]
"#
        ),
    )
}
