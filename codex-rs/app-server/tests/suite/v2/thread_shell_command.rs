use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::create_shell_command_sse_response;
use app_test_support::format_with_current_shell_display;
use app_test_support::to_response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionOutputDeltaNotification;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::EnvironmentAddResponse;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadShellCommandParams;
use codex_app_server_protocol::ThreadShellCommandResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnEnvironmentParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_core::shell::default_user_shell;
use codex_exec_server::CODEX_EXEC_SERVER_URL_ENV_VAR;
use codex_features::FEATURES;
use codex_features::Feature;
use codex_utils_path_uri::PathUri;
use futures::SinkExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use super::exec_server_test_support::accept_exec_server_environment;
use super::exec_server_test_support::read_exec_server_json;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_shell_command_history_responses_exclude_persisted_command_executions() -> Result<()>
{
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let server = create_mock_responses_server_sequence(vec![]).await;
    create_config_toml(
        codex_home.as_path(),
        &server.uri(),
        "never",
        &BTreeMap::default(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.as_path())
        // thread/shellCommand intentionally executes on the app-server host.
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;
    let (shell_command, expected_output) = current_shell_output_command("hello from bang")?;

    let shell_id = mcp
        .send_thread_shell_command_request(ThreadShellCommandParams {
            thread_id: thread.id.clone(),
            command: shell_command,
        })
        .await?;
    let shell_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(shell_id)),
    )
    .await??;
    let _: ThreadShellCommandResponse = to_response::<ThreadShellCommandResponse>(shell_resp)?;

    let started = wait_for_command_execution_started(&mut mcp, /*expected_id*/ None).await?;
    let ThreadItem::CommandExecution {
        id, source, status, ..
    } = &started.item
    else {
        unreachable!("helper returns command execution item");
    };
    let command_id = id.clone();
    assert_eq!(source, &CommandExecutionSource::UserShell);
    assert_eq!(status, &CommandExecutionStatus::InProgress);

    let delta = wait_for_command_execution_output_delta(&mut mcp, &command_id).await?;
    assert_eq!(
        delta.delta.trim_end_matches(['\r', '\n']),
        expected_output.trim_end_matches(['\r', '\n'])
    );

    let completed = wait_for_command_execution_completed(&mut mcp, Some(&command_id)).await?;
    let ThreadItem::CommandExecution {
        id,
        source,
        status,
        aggregated_output,
        exit_code,
        ..
    } = &completed.item
    else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(id, &command_id);
    assert_eq!(source, &CommandExecutionSource::UserShell);
    assert_eq!(status, &CommandExecutionStatus::Completed);
    assert_eq!(aggregated_output.as_deref(), Some(expected_output.as_str()));
    assert_eq!(*exit_code, Some(0));

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let read_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread.id.clone(),
            include_turns: true,
        })
        .await?;
    let read_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    let ThreadReadResponse { thread, .. } = to_response::<ThreadReadResponse>(read_resp)?;
    assert_eq!(thread.turns.len(), 1);
    assert_no_command_executions(&thread.turns[0].items, "thread/read");

    let turns_list_id = mcp
        .send_thread_turns_list_request(ThreadTurnsListParams {
            thread_id: thread.id.clone(),
            cursor: None,
            limit: None,
            sort_direction: Some(SortDirection::Asc),
            items_view: None,
        })
        .await?;
    let turns_list_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turns_list_id)),
    )
    .await??;
    let ThreadTurnsListResponse { data, .. } =
        to_response::<ThreadTurnsListResponse>(turns_list_resp)?;
    assert_eq!(data.len(), 1);
    assert_no_command_executions(&data[0].items, "thread/turns/list");

    let fork_id = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: thread.id,
            ..Default::default()
        })
        .await?;
    let fork_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(fork_id)),
    )
    .await??;
    let ThreadForkResponse { thread, .. } = to_response::<ThreadForkResponse>(fork_resp)?;
    assert_eq!(thread.turns.len(), 1);
    assert_no_command_executions(&thread.turns[0].items, "thread/fork");

    Ok(())
}

#[tokio::test]
async fn thread_shell_command_errors_when_thread_has_no_selected_environment() -> Result<()> {
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let server = create_mock_responses_server_sequence(vec![]).await;
    create_config_toml(
        codex_home.as_path(),
        &server.uri(),
        "never",
        &BTreeMap::default(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.as_path())
        // This test intentionally exercises thread/shellCommand without a local host environment.
        .without_auto_env()
        .with_env_overrides(&[(CODEX_EXEC_SERVER_URL_ENV_VAR, Some("none"))])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;
    let shell_id = mcp
        .send_thread_shell_command_request(ThreadShellCommandParams {
            thread_id: thread.id,
            command: "pwd".to_string(),
        })
        .await?;
    let error = mcp
        .read_stream_until_error_message(RequestId::Integer(shell_id))
        .await?;
    assert_eq!(error.error.message, "thread has no selected environment");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_shell_command_uses_selected_remote_environment_without_local_environment()
-> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let exec_server_url = format!("ws://{}", listener.local_addr()?);
    let exec_server = tokio::spawn(serve_remote_user_shell(listener));
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let server = create_mock_responses_server_sequence(vec![]).await;
    create_config_toml(
        codex_home.as_path(),
        &server.uri(),
        "never",
        &BTreeMap::default(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.as_path())
        .without_auto_env()
        .with_env_overrides(&[(CODEX_EXEC_SERVER_URL_ENV_VAR, Some("none"))])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let add_id = mcp
        .send_raw_request(
            "environment/add",
            Some(json!({
                "environmentId": "remote-a",
                "execServerUrl": exec_server_url,
                "connectTimeoutMs": 10_000,
            })),
        )
        .await?;
    let add_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(add_id)),
    )
    .await??;
    let _: EnvironmentAddResponse = to_response(add_response)?;

    let remote_cwd = PathUri::parse("file:///home/remote/workspace")?;
    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            environments: Some(vec![TurnEnvironmentParams {
                environment_id: "remote-a".to_string(),
                cwd: remote_cwd.clone().into(),
            }]),
            ..Default::default()
        })
        .await?;
    let start_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse {
        thread,
        selected_environment,
        ..
    } = to_response::<ThreadStartResponse>(start_response)?;
    assert_eq!(
        selected_environment.map(|selected| (selected.environment_id, selected.cwd)),
        Some(("remote-a".to_string(), remote_cwd))
    );

    let shell_id = mcp
        .send_thread_shell_command_request(ThreadShellCommandParams {
            thread_id: thread.id,
            command: "printf remote-shell".to_string(),
        })
        .await?;
    let shell_response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(shell_id)),
    )
    .await??;
    let _: ThreadShellCommandResponse = to_response(shell_response)?;

    let started = wait_for_command_execution_started(&mut mcp, /*expected_id*/ None).await?;
    let ThreadItem::CommandExecution { id, cwd, .. } = started.item else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(cwd.as_str(), "/home/remote/workspace");
    let completed = wait_for_command_execution_completed(&mut mcp, Some(&id)).await?;
    let ThreadItem::CommandExecution {
        status,
        aggregated_output,
        exit_code,
        ..
    } = completed.item
    else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(status, CommandExecutionStatus::Completed);
    assert_eq!(aggregated_output.as_deref(), Some("remote shell output\n"));
    assert_eq!(exit_code, Some(0));
    timeout(DEFAULT_READ_TIMEOUT, exec_server).await???;

    Ok(())
}

async fn serve_remote_user_shell(listener: TcpListener) -> Result<()> {
    let mut websocket = accept_exec_server_environment(
        listener,
        json!({
            "operatingSystem": "linux",
            "shell": {"name": "bash", "path": "/bin/bash"},
            "cwd": "file:///home/remote/workspace",
        }),
    )
    .await?;
    let mut shell_snapshot_process_id = None;
    let mut shell_process_id = None;

    loop {
        let request = read_exec_server_json(&mut websocket).await?;
        match request["method"].as_str() {
            Some("process/start") => {
                let process_id = request["params"]["processId"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("process/start missing processId"))?
                    .to_string();
                if process_id.contains("-shell-snapshot-") {
                    shell_snapshot_process_id = Some(process_id.clone());
                } else {
                    assert_eq!(request["params"]["cwd"], "file:///home/remote/workspace");
                    assert_eq!(
                        request["params"]["argv"]
                            .as_array()
                            .and_then(|argv| argv.last())
                            .and_then(Value::as_str),
                        Some("printf remote-shell")
                    );
                    shell_process_id = Some(process_id.clone());
                }
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": {"processId": process_id},
                    }),
                )
                .await?;
            }
            Some("process/read") => {
                let process_id = request["params"]["processId"].as_str();
                let result = if process_id == shell_snapshot_process_id.as_deref() {
                    json!({
                        "chunks": [],
                        "nextSeq": 0,
                        "exited": true,
                        "exitCode": 0,
                        "closed": true,
                        "failure": null,
                        "sandboxDenied": false,
                    })
                } else {
                    assert_eq!(process_id, shell_process_id.as_deref());
                    json!({
                        "chunks": [{
                            "seq": 1,
                            "stream": "stdout",
                            "chunk": BASE64_STANDARD.encode("remote shell output\n"),
                        }],
                        "nextSeq": 2,
                        "exited": true,
                        "exitCode": 0,
                        "closed": true,
                        "failure": null,
                        "sandboxDenied": false,
                    })
                };
                send_exec_server_json(
                    &mut websocket,
                    json!({"id": request["id"], "result": result}),
                )
                .await?;
                if process_id == shell_process_id.as_deref() {
                    return Ok(());
                }
            }
            Some("fs/createDirectory") => {
                send_exec_server_json(&mut websocket, json!({"id": request["id"], "result": {}}))
                    .await?;
            }
            Some("fs/getMetadata") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "error": {"code": -32004, "message": "not found"},
                    }),
                )
                .await?;
            }
            Some("fs/canonicalize") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": {"path": request["params"]["path"]},
                    }),
                )
                .await?;
            }
            Some("fs/walk") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": {"entries": [], "errors": [], "truncated": false},
                    }),
                )
                .await?;
            }
            method => anyhow::bail!("unexpected remote shell exec-server request: {method:?}"),
        }
    }
}

async fn send_exec_server_json(
    websocket: &mut WebSocketStream<TcpStream>,
    message: Value,
) -> Result<()> {
    websocket
        .send(Message::Text(message.to_string().into()))
        .await?;
    Ok(())
}

#[tokio::test]
async fn thread_shell_command_uses_existing_active_turn() -> Result<()> {
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let responses = vec![
        create_shell_command_sse_response(
            vec![
                "python3".to_string(),
                "-c".to_string(),
                "print(42)".to_string(),
            ],
            /*workdir*/ None,
            Some(5000),
            "call-approve",
        )?,
        create_final_assistant_message_sse_response("done")?,
    ];
    let server = create_mock_responses_server_sequence(responses).await;
    create_config_toml(
        codex_home.as_path(),
        &server.uri(),
        "untrusted",
        &BTreeMap::default(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.as_path())
        // thread/shellCommand intentionally joins the app-server's host-local active turn.
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let start_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_resp)?;
    let (shell_command, expected_output) = current_shell_output_command("active turn bang")?;

    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "run python".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(workspace.clone()),
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_id)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;

    let agent_started = wait_for_command_execution_started(&mut mcp, Some("call-approve")).await?;
    let ThreadItem::CommandExecution {
        command, source, ..
    } = &agent_started.item
    else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(source, &CommandExecutionSource::Agent);
    assert_eq!(
        command,
        &format_with_current_shell_display("python3 -c 'print(42)'")
    );

    let server_req = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::CommandExecutionRequestApproval { request_id, .. } = server_req else {
        panic!("expected approval request");
    };

    let shell_id = mcp
        .send_thread_shell_command_request(ThreadShellCommandParams {
            thread_id: thread.id.clone(),
            command: shell_command,
        })
        .await?;
    let shell_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(shell_id)),
    )
    .await??;
    let _: ThreadShellCommandResponse = to_response::<ThreadShellCommandResponse>(shell_resp)?;

    let started =
        wait_for_command_execution_started_by_source(&mut mcp, CommandExecutionSource::UserShell)
            .await?;
    assert_eq!(started.turn_id, turn.id);
    let command_id = match &started.item {
        ThreadItem::CommandExecution { id, .. } => id.clone(),
        _ => unreachable!("helper returns command execution item"),
    };
    let completed = wait_for_command_execution_completed(&mut mcp, Some(&command_id)).await?;
    assert_eq!(completed.turn_id, turn.id);
    let ThreadItem::CommandExecution {
        source,
        aggregated_output,
        ..
    } = &completed.item
    else {
        unreachable!("helper returns command execution item");
    };
    assert_eq!(source, &CommandExecutionSource::UserShell);
    assert_eq!(aggregated_output.as_deref(), Some(expected_output.as_str()));

    mcp.send_response(
        request_id,
        serde_json::to_value(CommandExecutionRequestApprovalResponse {
            decision: CommandExecutionApprovalDecision::Decline,
        })?,
    )
    .await?;
    let _: TurnCompletedNotification = serde_json::from_value(
        timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_notification_message("turn/completed"),
        )
        .await??
        .params
        .expect("turn/completed params"),
    )?;

    let read_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread.id,
            include_turns: true,
        })
        .await?;
    let read_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    let ThreadReadResponse { thread, .. } = to_response::<ThreadReadResponse>(read_resp)?;
    assert_eq!(thread.turns.len(), 1);
    assert_no_command_executions(&thread.turns[0].items, "thread/read");

    Ok(())
}

fn assert_no_command_executions(items: &[ThreadItem], context: &str) {
    assert!(
        items
            .iter()
            .all(|item| !matches!(item, ThreadItem::CommandExecution { .. })),
        "{context} should always exclude command executions from returned turns"
    );
}

fn current_shell_output_command(text: &str) -> Result<(String, String)> {
    let command_and_output = match default_user_shell().name() {
        "powershell" => {
            let escaped_text = text.replace('\'', "''");
            (
                format!("Write-Output '{escaped_text}'"),
                format!("{text}\r\n"),
            )
        }
        "cmd" => (format!("echo {text}"), format!("{text}\r\n")),
        _ => {
            let quoted_text = shlex::try_quote(text)?;
            (format!("printf '%s\\n' {quoted_text}"), format!("{text}\n"))
        }
    };
    Ok(command_and_output)
}

async fn wait_for_command_execution_started(
    mcp: &mut TestAppServer,
    expected_id: Option<&str>,
) -> Result<ItemStartedNotification> {
    loop {
        let notif = mcp
            .read_stream_until_notification_message("item/started")
            .await?;
        let started: ItemStartedNotification = serde_json::from_value(
            notif
                .params
                .ok_or_else(|| anyhow::anyhow!("missing item/started params"))?,
        )?;
        let ThreadItem::CommandExecution { id, .. } = &started.item else {
            continue;
        };
        if expected_id.is_none() || expected_id == Some(id.as_str()) {
            return Ok(started);
        }
    }
}

async fn wait_for_command_execution_started_by_source(
    mcp: &mut TestAppServer,
    expected_source: CommandExecutionSource,
) -> Result<ItemStartedNotification> {
    loop {
        let started = wait_for_command_execution_started(mcp, /*expected_id*/ None).await?;
        let ThreadItem::CommandExecution { source, .. } = &started.item else {
            continue;
        };
        if source == &expected_source {
            return Ok(started);
        }
    }
}

async fn wait_for_command_execution_completed(
    mcp: &mut TestAppServer,
    expected_id: Option<&str>,
) -> Result<ItemCompletedNotification> {
    loop {
        let notif = mcp
            .read_stream_until_notification_message("item/completed")
            .await?;
        let completed: ItemCompletedNotification = serde_json::from_value(
            notif
                .params
                .ok_or_else(|| anyhow::anyhow!("missing item/completed params"))?,
        )?;
        let ThreadItem::CommandExecution { id, .. } = &completed.item else {
            continue;
        };
        if expected_id.is_none() || expected_id == Some(id.as_str()) {
            return Ok(completed);
        }
    }
}

async fn wait_for_command_execution_output_delta(
    mcp: &mut TestAppServer,
    item_id: &str,
) -> Result<CommandExecutionOutputDeltaNotification> {
    loop {
        let notif = mcp
            .read_stream_until_notification_message("item/commandExecution/outputDelta")
            .await?;
        let delta: CommandExecutionOutputDeltaNotification = serde_json::from_value(
            notif
                .params
                .ok_or_else(|| anyhow::anyhow!("missing output delta params"))?,
        )?;
        if delta.item_id == item_id {
            return Ok(delta);
        }
    }
}

fn create_config_toml(
    codex_home: &Path,
    server_uri: &str,
    approval_policy: &str,
    feature_flags: &BTreeMap<Feature, bool>,
) -> std::io::Result<()> {
    let feature_entries = feature_flags
        .iter()
        .map(|(feature, enabled)| {
            let key = FEATURES
                .iter()
                .find(|spec| spec.id == *feature)
                .map(|spec| spec.key)
                .expect("feature should have a config key");
            format!("{key} = {enabled}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "{approval_policy}"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[features]
{feature_entries}

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
