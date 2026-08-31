use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_exec_server::REMOTE_ENVIRONMENT_ID;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const CALL_ID: &str = "pushed-remote-process-events";
const COMPLETE_OUTPUT: &str = "pushed remote output\n";
const RECOVERED_OUTPUT: &str = "recovered missing output\n";
const RETAINED_OUTPUT: &str = "retained output\n";
const REPLAY_OUTPUT_EVENT_COUNT: u64 = 1024;
const REPLAY_RETAINED_OUTPUT_SEQ: u64 = 800;
const PUSHED_EXEC_EVENT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
enum PushedExecScenario {
    Complete,
    LegacyShellAdapter,
    RemoteApproval,
    DirectDenied,
    LegacyExit,
    ReplayGap,
}

async fn read_exec_server_json(websocket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        match timeout(PUSHED_EXEC_EVENT_TIMEOUT, websocket.next())
            .await
            .expect("websocket read should not time out")
            .expect("websocket should stay open")
            .expect("websocket frame should read")
        {
            Message::Text(text) => {
                return serde_json::from_str(text.as_ref()).expect("valid JSON-RPC message");
            }
            Message::Binary(bytes) => {
                return serde_json::from_slice(bytes.as_ref()).expect("valid JSON-RPC message");
            }
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected JSON-RPC message, got {other:?}"),
        }
    }
}

async fn try_read_exec_server_json(
    websocket: &mut WebSocketStream<TcpStream>,
    wait: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let frame = tokio::time::timeout_at(deadline, websocket.next())
            .await
            .ok()??
            .ok()?;
        match frame {
            Message::Text(text) => {
                return Some(serde_json::from_str(text.as_ref()).expect("valid JSON-RPC message"));
            }
            Message::Binary(bytes) => {
                return Some(
                    serde_json::from_slice(bytes.as_ref()).expect("valid JSON-RPC message"),
                );
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => return None,
            other => panic!("expected JSON-RPC message, got {other:?}"),
        }
    }
}

async fn send_exec_server_json(websocket: &mut WebSocketStream<TcpStream>, message: Value) {
    websocket
        .send(Message::Text(message.to_string().into()))
        .await
        .expect("exec-server message should send");
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

async fn accept_initialized_exec_server(listener: TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("connection");
    let mut websocket = accept_async(stream).await.expect("websocket handshake");

    let initialize = read_exec_server_json(&mut websocket).await;
    assert_eq!(initialize["method"], "initialize");
    send_exec_server_json(
        &mut websocket,
        json!({
            "id": initialize["id"],
            "result": { "sessionId": "test-session" }
        }),
    )
    .await;
    let initialized = read_exec_server_json(&mut websocket).await;
    assert_eq!(initialized["method"], "initialized");

    websocket
}

async fn send_environment_info(
    websocket: &mut WebSocketStream<TcpStream>,
) {
    let info = read_exec_server_json(websocket).await;
    assert_eq!(info["method"], "environment/info");
    send_exec_server_json(
        websocket,
        json!({
            "id": info["id"],
            "result": {
                "operatingSystem": "windows",
                "shell": {
                    "name": "powershell",
                    "path": "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
                },
                "cwd": "file:///C:/workspace"
            },
        }),
    )
    .await;
}

async fn serve_exec_with_pushed_events(
    listener: TcpListener,
    scenario: PushedExecScenario,
) -> usize {
    let mut websocket = accept_initialized_exec_server(listener).await;
    send_environment_info(&mut websocket).await;

    let mut snapshot_process_id = None;
    let process_start = loop {
        let Some(request) =
            try_read_exec_server_json(&mut websocket, PUSHED_EXEC_EVENT_TIMEOUT).await
        else {
            return 0;
        };
        match request["method"].as_str() {
            Some("process/start")
                if request["params"]["processId"]
                    .as_str()
                    .is_some_and(|process_id| process_id.contains("-shell-snapshot-")) =>
            {
                let process_id = request["params"]["processId"]
                    .as_str()
                    .expect("shell snapshot process should include processId")
                    .to_string();
                snapshot_process_id = Some(process_id.clone());
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": { "processId": process_id }
                    }),
                )
                .await;
                for message in [
                    json!({
                        "method": "process/exited",
                        "params": {
                            "processId": &process_id,
                            "seq": 1,
                            "exitCode": 0,
                            "sandboxDenied": false,
                        }
                    }),
                    json!({
                        "method": "process/closed",
                        "params": { "processId": &process_id, "seq": 2 }
                    }),
                ] {
                    send_exec_server_json(&mut websocket, message).await;
                }
            }
            Some("process/start") => break request,
            Some("process/read")
                if snapshot_process_id.as_deref() == request["params"]["processId"].as_str() =>
            {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": {
                            "chunks": [],
                            "nextSeq": 0,
                            "exited": true,
                            "exitCode": 0,
                            "closed": true,
                            "failure": null,
                            "sandboxDenied": false,
                        }
                    }),
                )
                .await;
            }
            Some("fs/createDirectory") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": {}
                    }),
                )
                .await;
            }
            Some("fs/getMetadata") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "error": { "code": -32004, "message": "not found" }
                    }),
                )
                .await;
            }
            Some("fs/canonicalize") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": { "path": request["params"]["path"] }
                    }),
                )
                .await;
            }
            Some("fs/walk") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": { "entries": [], "errors": [], "truncated": false }
                    }),
                )
                .await;
            }
            method => panic!("unexpected exec-server request before process/start: {method:?}"),
        }
    };
    let process_id = process_start["params"]["processId"]
        .as_str()
        .expect("process/start should include processId")
        .to_string();
    if matches!(scenario, PushedExecScenario::LegacyShellAdapter) {
        assert_eq!(
            process_start["params"]["cwd"],
            "file:///home/remote/workspace/nested"
        );
        assert!(
            process_start["params"]["argv"]
                .as_array()
                .and_then(|argv| argv.last())
                .and_then(Value::as_str)
                .is_some_and(|script| script.ends_with("ignored by fake exec-server"))
        );
    }

    let replay_output = |seq| -> &'static [u8] {
        match seq {
            1 => RECOVERED_OUTPUT.as_bytes(),
            REPLAY_RETAINED_OUTPUT_SEQ => RETAINED_OUTPUT.as_bytes(),
            _ => b"x",
        }
    };
    if matches!(scenario, PushedExecScenario::ReplayGap) {
        // The process replay log retains 256 events. This burst is much larger
        // than both that log and the JSON-RPC event queue, so the reader must
        // apply enough notifications to evict seq 1 before it can read the
        // start response. The total output stays well below the server's 1 MiB
        // retained-output limit, making the subsequent read genuinely able to
        // recover every missing chunk.
        for seq in 1..=REPLAY_OUTPUT_EVENT_COUNT {
            send_exec_server_json(
                &mut websocket,
                json!({
                    "method": "process/output",
                    "params": {
                        "processId": &process_id,
                        "seq": seq,
                        "stream": "stdout",
                        "chunk": BASE64_STANDARD.encode(replay_output(seq)),
                    }
                }),
            )
            .await;
        }
        send_exec_server_json(
            &mut websocket,
            json!({
                "method": "process/exited",
                "params": {
                    "processId": &process_id,
                    "seq": REPLAY_OUTPUT_EVENT_COUNT + 1,
                    "exitCode": 0,
                    "sandboxDenied": false,
                }
            }),
        )
        .await;
    }

    send_exec_server_json(
        &mut websocket,
        json!({
            "id": process_start["id"],
            "result": { "processId": &process_id }
        }),
    )
    .await;

    match scenario {
        PushedExecScenario::Complete
        | PushedExecScenario::LegacyShellAdapter
        | PushedExecScenario::RemoteApproval => {
            let encoded_output = BASE64_STANDARD.encode(COMPLETE_OUTPUT);
            for message in [
                json!({
                    "method": "process/output",
                    "params": {
                        "processId": &process_id,
                        "seq": 1,
                        "stream": "stdout",
                        "chunk": encoded_output,
                    }
                }),
                json!({
                    "method": "process/exited",
                    "params": {
                        "processId": &process_id,
                        "seq": 2,
                        "exitCode": 0,
                        "sandboxDenied": false,
                    }
                }),
                json!({
                    "method": "process/closed",
                    "params": { "processId": &process_id, "seq": 3 }
                }),
            ] {
                send_exec_server_json(&mut websocket, message).await;
            }
        }
        PushedExecScenario::DirectDenied => {
            send_exec_server_json(
                &mut websocket,
                json!({
                    "method": "process/exited",
                    "params": {
                        "processId": &process_id,
                        "seq": 1,
                        "exitCode": 1,
                        "sandboxDenied": true,
                    }
                }),
            )
            .await;
        }
        PushedExecScenario::LegacyExit => {
            send_exec_server_json(
                &mut websocket,
                json!({
                    "method": "process/exited",
                    "params": {
                        "processId": &process_id,
                        "seq": 1,
                        "exitCode": 1,
                    }
                }),
            )
            .await;
        }
        PushedExecScenario::ReplayGap => {}
    }

    let mut process_read_requests = 0;
    loop {
        let Some(request) = try_read_exec_server_json(&mut websocket, Duration::from_secs(2)).await
        else {
            return process_read_requests;
        };
        match request["method"].as_str() {
            Some("process/read") => {
                process_read_requests += 1;
                let result = match scenario {
                    PushedExecScenario::Complete
                    | PushedExecScenario::LegacyShellAdapter
                    | PushedExecScenario::RemoteApproval => json!({
                        "chunks": [{
                            "seq": 1,
                            "stream": "stdout",
                            "chunk": BASE64_STANDARD.encode(COMPLETE_OUTPUT),
                        }],
                        "nextSeq": 4,
                        "exited": true,
                        "exitCode": 0,
                        "closed": true,
                        "failure": null,
                        "sandboxDenied": false,
                    }),
                    PushedExecScenario::DirectDenied => json!({
                        "chunks": [],
                        "nextSeq": 2,
                        "exited": true,
                        "exitCode": 1,
                        "closed": false,
                        "failure": null,
                        "sandboxDenied": true,
                    }),
                    PushedExecScenario::LegacyExit => json!({
                        "chunks": [],
                        "nextSeq": 3,
                        "exited": true,
                        "exitCode": 1,
                        "closed": true,
                        "failure": null,
                        "sandboxDenied": true,
                    }),
                    PushedExecScenario::ReplayGap => {
                        let chunks = (1..=REPLAY_OUTPUT_EVENT_COUNT)
                            .map(|seq| {
                                json!({
                                    "seq": seq,
                                    "stream": "stdout",
                                    "chunk": BASE64_STANDARD.encode(replay_output(seq)),
                                })
                            })
                            .collect::<Vec<_>>();
                        json!({
                            "chunks": chunks,
                            "nextSeq": REPLAY_OUTPUT_EVENT_COUNT + 2,
                            "exited": true,
                            "exitCode": 0,
                            "closed": false,
                            "failure": null,
                            "sandboxDenied": false,
                        })
                    }
                };
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": result,
                    }),
                )
                .await;
                if matches!(scenario, PushedExecScenario::ReplayGap) && process_read_requests == 1 {
                    send_exec_server_json(
                        &mut websocket,
                        json!({
                            "method": "process/closed",
                            "params": {
                                "processId": &process_id,
                                "seq": REPLAY_OUTPUT_EVENT_COUNT + 2,
                            }
                        }),
                    )
                    .await;
                    return process_read_requests;
                }
                if matches!(scenario, PushedExecScenario::LegacyExit) {
                    return process_read_requests;
                }
            }
            Some("process/terminate") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": { "running": false }
                    }),
                )
                .await;
                return process_read_requests;
            }
            method => panic!("unexpected exec-server request: {method:?}"),
        }
    }
}

#[test_case(PushedExecScenario::Complete ; "complete_event_stream")]
#[test_case(PushedExecScenario::LegacyShellAdapter ; "legacy_shell_adapter_in_foreign_environment")]
#[test_case(PushedExecScenario::RemoteApproval ; "approval_in_foreign_environment")]
#[test_case(PushedExecScenario::DirectDenied ; "direct_sandbox_denial")]
#[test_case(PushedExecScenario::LegacyExit ; "legacy_exit_metadata")]
#[test_case(PushedExecScenario::ReplayGap ; "truncated_event_replay")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_command_consumes_pushed_remote_process_events(
    scenario: PushedExecScenario,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let tool_name = if matches!(scenario, PushedExecScenario::LegacyShellAdapter) {
        "shell_command"
    } else {
        "exec_command"
    };
    let tool_arguments = match scenario {
        PushedExecScenario::LegacyShellAdapter => json!({
            "kind": "script",
            "command": "ignored by fake exec-server",
            "workdir": "nested",
            "timeout_ms": 1_000,
        }),
        PushedExecScenario::RemoteApproval => json!({
            "kind": "script",
            "cmd": "ignored by fake exec-server",
            "yield_time_ms": 1_000,
            "sandbox_permissions": "require_escalated",
            "justification": "exercise remote approval cwd",
        }),
        _ => json!({
            "kind": "script",
            "cmd": "ignored by fake exec-server",
            "yield_time_ms": 1_000,
        }),
    };
    let mut responses = vec![sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(CALL_ID, tool_name, &tool_arguments.to_string()),
        ev_completed("resp-1"),
    ])];
    if matches!(
        scenario,
        PushedExecScenario::Complete
            | PushedExecScenario::LegacyShellAdapter
            | PushedExecScenario::RemoteApproval
            | PushedExecScenario::ReplayGap
    ) {
        responses.push(sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-2", "done"),
            ev_completed("resp-2"),
        ]));
    }
    let response_mock = mount_sse_sequence(&server, responses).await;
    let exec_server_url = format!("ws://{}", listener.local_addr()?);
    let exec_server = tokio::spawn(serve_exec_with_pushed_events(listener, scenario));
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.5", |model_info| {
            model_info.tool_mode = Some(ToolMode::Direct);
            model_info.shell_type = ConfigShellToolType::ShellCommand;
        })
        .with_exec_server_url(exec_server_url)
        .with_config(move |config| {
            config.project_doc_max_bytes = 0;
            config
                .features
                .disable(Feature::DeferredExecutor)
                .expect("test config should wait for the selected exec server");
            config
                .permissions
                .set_permission_profile(PermissionProfile::Disabled)
                .expect("test config should allow an unsandboxed permission profile");
            config.permissions.network = None;
            config
                .features
                .enable(Feature::ShellTool)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::UnifiedExec)
                .expect("test config should allow feature update");
        });
    let test = timeout(Duration::from_secs(5), builder.build(&server))
        .await
        .context("thread startup should connect to the fake exec-server")??;

    // Foreign path conventions cannot seed a host sandbox. The approval case
    // still exercises the explicit escalation flow, but its initial profile
    // must be unsandboxed so the remote cwd can reach the exec server.
    let profile = PermissionProfile::Disabled;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(profile, test.config.cwd.as_path());
    let remote_cwd = if matches!(
        scenario,
        PushedExecScenario::LegacyShellAdapter | PushedExecScenario::RemoteApproval
    ) {
        "file:///home/remote/workspace"
    } else {
        "file:///C:/workspace"
    };
    let environments = TurnEnvironmentSelections::new(
        test.config.cwd.clone(),
        vec![TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::parse(remote_cwd)?,
        }],
    );
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "run a one-shot remote command".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(environments),
                approval_policy: Some(if matches!(scenario, PushedExecScenario::RemoteApproval) {
                    AskForApproval::OnRequest
                } else {
                    AskForApproval::Never
                }),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    let mut saw_exec_command_begin = false;
    loop {
        let event = timeout(Duration::from_secs(5), test.codex.next_event())
            .await
            .context("turn should complete")??
            .msg;
        match event {
            EventMsg::ExecCommandBegin(event) if event.call_id == CALL_ID => {
                saw_exec_command_begin = true;
            }
            EventMsg::ExecApprovalRequest(event) if event.call_id == CALL_ID => {
                assert!(matches!(scenario, PushedExecScenario::RemoteApproval));
                assert_eq!(
                    event.cwd_uri.as_ref().map(ToString::to_string).as_deref(),
                    Some("file:///home/remote/workspace")
                );
                test.codex
                    .submit(Op::ExecApproval {
                        id: event.effective_approval_id(),
                        turn_id: None,
                        decision: ReviewDecision::Approved,
                    })
                    .await?;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    let process_read_requests = timeout(Duration::from_secs(5), exec_server)
        .await
        .context("fake exec-server should observe process cleanup")??;
    if matches!(
        scenario,
        PushedExecScenario::DirectDenied | PushedExecScenario::LegacyExit
    ) {
        assert!(saw_exec_command_begin);
        assert_eq!(response_mock.requests().len(), 1);
        assert_eq!(
            process_read_requests,
            usize::from(matches!(scenario, PushedExecScenario::LegacyExit)),
            "legacy exit metadata should require one compatibility read"
        );
        return Ok(());
    }
    let request = response_mock
        .last_request()
        .context("model should receive the exec_command output")?;
    let advertised_tools = response_mock
        .requests()
        .first()
        .map(|request| tool_names(&request.body_json()))
        .unwrap_or_default();
    let (output, success) = request
        .function_call_output_content_and_success(CALL_ID)
        .context("exec_command output should be model visible")?;
    let output = output.context("exec_command output should contain text")?;
    match scenario {
        PushedExecScenario::Complete
        | PushedExecScenario::LegacyShellAdapter
        | PushedExecScenario::RemoteApproval => {
            assert_ne!(success, Some(false), "remote execution failed: {output}");
            assert!(
                saw_exec_command_begin,
                "remote execution did not begin: {output}; advertised tools: {advertised_tools:?}"
            );
            assert!(output.contains("Process exited with code 0"));
            assert!(output.contains(COMPLETE_OUTPUT));
            assert_eq!(process_read_requests, 0, "unexpected compatibility read");
        }
        PushedExecScenario::DirectDenied | PushedExecScenario::LegacyExit => unreachable!(),
        PushedExecScenario::ReplayGap => {
            assert_ne!(success, Some(false));
            assert!(saw_exec_command_begin);
            assert_eq!(output.matches(RECOVERED_OUTPUT).count(), 1);
            assert_eq!(output.matches(RETAINED_OUTPUT).count(), 1);
            assert_eq!(process_read_requests, 1, "expected replay recovery read");
        }
    }

    Ok(())
}
