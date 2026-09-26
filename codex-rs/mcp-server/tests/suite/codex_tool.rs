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

use core_test_support::require_network;
use mcp_test_support::McpProcess;
use mcp_test_support::create_final_assistant_message_sse_response;
use mcp_test_support::create_mock_responses_server;
use mcp_test_support::create_shell_command_sse_response;
use mcp_test_support::format_with_current_shell;

// Windows CI can spend tens of seconds in session startup before the first
// mock model request is sent.
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_interactive_tools_abort_instead_of_hanging() -> anyhow::Result<()> {
    use core_test_support::responses::ev_completed;
    use core_test_support::responses::ev_function_call;
    use core_test_support::responses::ev_response_created;
    use core_test_support::responses::sse;
    require_network!();
    for (tool, arguments) in [
        (
            "request_user_input",
            json!({"questions":[{"id":"choice","header":"Choice","question":"Which?","options":[{"label":"A","description":"First"},{"label":"B","description":"Second"}]}]}),
        ),
        (
            "request_permissions",
            json!({"reason":"Need network", "permissions":{"network":{"enabled":true}}}),
        ),
    ] {
        let McpHandle {
            mut process,
            server,
            dir: _dir,
        } = create_mcp_process(vec![sse(vec![
            ev_response_created("interactive"),
            ev_function_call("interactive-call", tool, &arguments.to_string()),
            ev_completed("interactive"),
        ])])
        .await?;
        let id = process.send_request("tools/call", Some(json!({"name":"codex","arguments":{
            "prompt":"Use the requested interactive tool", "approval-policy":"on-request",
            "sandbox":"read-only", "config":{"features.default_mode_request_user_input":true,"features.request_permissions_tool":true}
        }}))).await?;
        let response = timeout(
            DEFAULT_READ_TIMEOUT,
            process.read_stream_until_response_message(RequestId::Number(id)),
        )
        .await??;
        assert_eq!(response.result["isError"], true);
        assert_eq!(
            response.result["content"][0]["text"],
            format!("{tool} is not supported by the MCP server.")
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .ok_or_else(|| anyhow::anyhow!("request recording is disabled"))?
                .len(),
            1
        );
        process.close_stdin();
        assert!(
            timeout(std::time::Duration::from_secs(5), process.wait_for_exit())
                .await??
                .success()
        );
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_methods_and_malformed_frames_respond_and_keep_transport_usable()
-> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let mut process = McpProcess::new(codex_home.path()).await?;
    for (method, params) in [
        ("resources/list", json!({})),
        ("resources/templates/list", json!({})),
        ("resources/read", json!({"uri":"test://resource"})),
        ("resources/subscribe", json!({"uri":"test://resource"})),
        ("resources/unsubscribe", json!({"uri":"test://resource"})),
        ("prompts/list", json!({})),
        ("prompts/get", json!({"name":"test"})),
        ("logging/setLevel", json!({"level":"info"})),
        (
            "completion/complete",
            json!({"ref":{"type":"ref/prompt","name":"test"},"argument":{"name":"query","value":"x"}}),
        ),
    ] {
        let id = process.send_request(method, Some(params)).await?;
        let response = timeout(DEFAULT_READ_TIMEOUT, process.read_jsonrpc_message()).await??;
        let rmcp::model::JsonRpcMessage::Error(error) = response else {
            anyhow::bail!("expected method error for {method}: {response:?}");
        };
        assert_eq!(error.id, Some(RequestId::Number(id)));
        assert_eq!(error.error.code, rmcp::model::ErrorCode::METHOD_NOT_FOUND);
    }
    for (frame, code, id) in [
        ("{", rmcp::model::ErrorCode::PARSE_ERROR, None),
        (
            r#"{"jsonrpc":"2.0","id":500,"method":42}"#,
            rmcp::model::ErrorCode::INVALID_REQUEST,
            Some(RequestId::Number(500)),
        ),
    ] {
        process.send_raw_frame(frame).await?;
        let response = timeout(DEFAULT_READ_TIMEOUT, process.read_jsonrpc_message()).await??;
        let rmcp::model::JsonRpcMessage::Error(error) = response else {
            anyhow::bail!("expected frame error: {response:?}");
        };
        assert_eq!(error.id, id);
        assert_eq!(error.error.code, code);
    }
    let ping = process.send_ping_request().await?;
    process.close_stdin();
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        process.read_stream_until_response_message(RequestId::Number(ping)),
    )
    .await??;
    assert_eq!(response.result, json!({}));
    assert!(
        timeout(DEFAULT_READ_TIMEOUT, process.wait_for_exit())
            .await??
            .success()
    );
    Ok(())
}

#[tokio::test]
async fn eof_bounds_shutdown_when_stdout_is_open_but_not_drained() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let mut process = McpProcess::new(codex_home.path()).await?;
    let ping = process.send_ping_request().await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        process.read_stream_until_response_message(RequestId::Number(ping)),
    )
    .await??;
    assert_eq!(response.result, json!({}));
    // These responses exceed pipe capacity while fitting in the bounded queue.
    for _ in 0..64 {
        process.send_request("tools/list", None).await?;
    }
    process.close_stdin();
    let status = timeout(DEFAULT_READ_TIMEOUT, process.wait_for_exit()).await??;
    assert!(
        !status.success(),
        "undrained stdout must report a timeout: {status}"
    );
    Ok(())
}

#[tokio::test]
async fn initialize_negotiates_unknown_protocol_versions() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let mut process = McpProcess::new(codex_home.path()).await?;
    let id = process
        .send_request(
            "initialize",
            Some(json!({
                "protocolVersion":"2099-01-01", "capabilities":{},
                "clientInfo":{"name":"test","version":"1"},
            })),
        )
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        process.read_stream_until_response_message(RequestId::Number(id)),
    )
    .await??;
    assert_eq!(response.result["protocolVersion"], "2025-11-25");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_without_form_support_denies_command_and_completes() -> anyhow::Result<()> {
    require_network!();
    let workdir = TempDir::new()?;
    let server = create_mock_responses_server(vec![
        create_shell_command_sse_response(
            vec![
                "New-Item".into(),
                "-ItemType".into(),
                "File".into(),
                "denied.txt".into(),
            ],
            Some(workdir.path()),
            Some(10_000),
            "unsupported-approval",
        )?,
        create_final_assistant_message_sse_response("The command was denied.")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let mut process = McpProcess::new(codex_home.path()).await?;
    let init = process
        .send_request(
            "initialize",
            Some(json!({
                "protocolVersion":"2025-11-25", "capabilities":{"elicitation":{"url":{}}},
                "clientInfo":{"name":"url-only","version":"1"},
            })),
        )
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        process.read_stream_until_response_message(RequestId::Number(init)),
    )
    .await??;
    let id = process
        .send_codex_tool_call(CodexToolCallParam {
            prompt: "Create the file".into(),
            cwd: Some(workdir.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let result = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            match process.read_jsonrpc_message().await? {
                rmcp::model::JsonRpcMessage::Request(request) => {
                    anyhow::bail!("unsupported elicitation was sent: {request:?}")
                }
                rmcp::model::JsonRpcMessage::Response(response)
                    if response.id == RequestId::Number(id) =>
                {
                    break Ok::<_, anyhow::Error>(response.result);
                }
                _ => {}
            }
        }
    })
    .await??;
    assert_eq!(result["content"][0]["text"], "The command was denied.");
    assert_ne!(result["isError"], true);
    assert!(!workdir.path().join("denied.txt").exists());
    assert_denial_reported_to_model(&server, "unsupported-approval").await?;
    Ok(())
}

/// A denied command is not run; the rejection is returned to the model, which
/// makes exactly one follow-up request to finish the turn.
async fn assert_denial_reported_to_model(server: &MockServer, call_id: &str) -> anyhow::Result<()> {
    let requests = server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("request recording is disabled"))?;
    assert_eq!(requests.len(), 2);
    let follow_up = requests[1].body_json::<serde_json::Value>()?;
    let output = follow_up["input"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        })
        .ok_or_else(|| anyhow::anyhow!("follow-up request lacks the denied call's output"))?;
    assert!(
        output["output"].to_string().contains("rejected by user"),
        "denied call output should report the rejection: {output}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_failure_shuts_down_with_stdin_still_open() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp_process = McpProcess::new(codex_home.path()).await?;
    let ping = mcp_process.send_ping_request().await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_response_message(RequestId::Number(ping)),
    )
    .await??;
    assert_eq!(response.result, json!({}));
    mcp_process.close_stdout();
    mcp_process.send_ping_request().await?;

    let status = timeout(DEFAULT_READ_TIMEOUT, mcp_process.wait_for_exit()).await??;
    assert!(
        !status.success(),
        "a broken stdout must fail the MCP process: {status}"
    );
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
    shell_command_approval_triggers_elicitation(true)
        .await
        .expect("shell command approval should trigger elicitation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_command_approval_client_error_denies_command_and_completes() -> anyhow::Result<()> {
    require_network!();
    shell_command_approval_triggers_elicitation(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_command_approval_cancellation_completes_and_ignores_late_approval()
-> anyhow::Result<()> {
    require_network!();
    let workdir = TempDir::new()?;
    let created_file = workdir.path().join("must_not_be_created.txt");
    let command = vec![
        "New-Item".to_string(),
        "-ItemType".to_string(),
        "File".to_string(),
        "-Path".to_string(),
        "must_not_be_created.txt".to_string(),
        "-Force".to_string(),
    ];
    let McpHandle {
        process: mut mcp_process,
        server: _server,
        dir: _dir,
    } = create_mcp_process(vec![create_shell_command_sse_response(
        command,
        Some(workdir.path()),
        Some(10_000),
        "cancelled-call",
    )?])
    .await?;
    let request_id = mcp_process
        .send_codex_tool_call(CodexToolCallParam {
            prompt: "Create the file".to_string(),
            cwd: Some(workdir.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let approval = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_request_message(),
    )
    .await??;
    assert_eq!(approval.request.method, "elicitation/create");

    mcp_process.cancel_tool_call(request_id).await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_response_message(RequestId::Number(request_id)),
    )
    .await??;
    assert_eq!(response.result["isError"], json!(true));
    assert_eq!(
        response.result["content"],
        json!([{ "type": "text", "text": "Turn aborted." }])
    );
    assert!(
        !created_file.exists(),
        "cancelled approval must not run the shell command"
    );

    mcp_process
        .send_response(
            approval.id,
            serde_json::to_value(ExecApprovalResponse {
                decision: ReviewDecision::Approved,
            })?,
        )
        .await?;
    let ping = mcp_process.send_ping_request().await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp_process.read_stream_until_response_message(RequestId::Number(ping)),
    )
    .await??;
    assert_eq!(response.result, json!({}));
    assert!(
        !created_file.exists(),
        "a late approval must not revive the cancelled command"
    );
    Ok(())
}

async fn shell_command_approval_triggers_elicitation(approve: bool) -> anyhow::Result<()> {
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

    let final_message = if approve {
        "File created!"
    } else {
        "The command was denied."
    };
    let responses = vec![
        create_shell_command_sse_response(
            shell_command.clone(),
            Some(workdir_for_shell_function_call.path()),
            Some(timeout_ms),
            "call1234",
        )?,
        create_final_assistant_message_sse_response(final_message)?,
    ];
    let McpHandle {
        process: mut mcp_process,
        server,
        dir: _dir,
    } = create_mcp_process(responses).await?;

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
    let mut expected_params = create_expected_elicitation_request_params(
        expected_shell_command,
        workdir_for_shell_function_call.path(),
        codex_request_id.to_string(),
        params.codex_event_id.clone(),
        params.thread_id,
    )?;
    assert!(
        params.message.starts_with(
            expected_params["message"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("expected elicitation message is missing"))?
        )
    );
    assert!(params.message.contains("\nWorking directory URI:"));
    assert!(
        params
            .message
            .contains("\nAllowed decisions: [\"approved\"")
    );
    expected_params["message"] = json!(params.message);
    assert_eq!(elicitation_request.request.params, Some(expected_params));

    // Accept the `git init` request by responding to the elicitation.
    if approve {
        mcp_process
            .send_response(
                elicitation_request_id,
                serde_json::to_value(ExecApprovalResponse {
                    decision: ReviewDecision::Approved,
                })?,
            )
            .await?;
    } else {
        mcp_process
            .send_error(
                elicitation_request_id,
                rmcp::model::ErrorData::internal_error("approval UI unavailable", None),
            )
            .await?;
    }

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
                        "text": final_message,
                        "type": "text"
                    }
                ],
                "structuredContent": {
                    "threadId": params.thread_id,
                    "content": final_message
                }
            }),
        },
        codex_response
    );

    if approve {
        let requests = server
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("request recording is disabled"))?;
        assert_eq!(requests.len(), 2);
    } else {
        assert_denial_reported_to_model(&server, "call1234").await?;
    }

    assert_eq!(
        created_file.is_file(),
        approve,
        "a failed approval must not execute the command"
    );

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
    require_network!();

    // Apparently `#[tokio::test]` must return `()`, so we create a helper
    // function that returns `Result` so we can use `?` in favor of `unwrap`.
    codex_tool_passes_base_instructions()
        .await
        .expect("codex tool should pass base instructions");
}

async fn codex_tool_passes_base_instructions() -> anyhow::Result<()> {
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

    let requests = server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("request recording is disabled"))?;
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
