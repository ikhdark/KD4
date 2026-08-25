use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_config::types::ApprovalsReviewer;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_core::config::Constrained;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::REMOTE_ENVIRONMENT_ID;
use codex_exec_server::RemoveOptions;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use core_test_support::TestTargetOs;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_remote_env;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::test_env;
use core_test_support::test_docker_container_name;
use core_test_support::test_target_os;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
#[allow(dead_code)]
async fn unified_exec_test(server: &wiremock::MockServer) -> Result<TestCodex> {
    let mut builder = test_codex().with_config(|config| {
        config.use_experimental_unified_exec_tool = true;
        let result = config.features.enable(Feature::UnifiedExec);
        assert!(
            result.is_ok(),
            "unified exec should enable for test: {result:?}",
        );
    });
    builder.build_with_remote_and_local_env(server).await
}

#[allow(dead_code)]
async fn submit_turn_with_approval_and_environments(
    test: &TestCodex,
    prompt: &str,
    environments: Vec<TurnEnvironmentSelection>,
    approval_policy: AskForApproval,
) -> Result<()> {
    let turn_environment_selections = codex_protocol::protocol::TurnEnvironmentSelections::new(
        test.config.cwd.clone(),
        environments,
    );
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(turn_environment_selections),
                approval_policy: Some(approval_policy),
                approvals_reviewer: Some(ApprovalsReviewer::User),
                sandbox_policy: Some(SandboxPolicy::new_read_only_policy()),
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

    Ok(())
}

#[allow(dead_code)]
async fn expect_patch_approval(
    test: &TestCodex,
    expected_call_id: &str,
) -> ApplyPatchApprovalRequestEvent {
    let event = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ApplyPatchApprovalRequest(_) | EventMsg::TurnComplete(_)
        )
    })
    .await;

    match event {
        EventMsg::ApplyPatchApprovalRequest(approval) => {
            assert_eq!(approval.call_id, expected_call_id);
            approval
        }
        EventMsg::TurnComplete(_) => panic!("expected patch approval request before completion"),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[allow(dead_code)]
async fn wait_for_completion_without_patch_approval(test: &TestCodex) {
    let event = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ApplyPatchApprovalRequest(_) | EventMsg::TurnComplete(_)
        )
    })
    .await;

    match event {
        EventMsg::TurnComplete(_) => {}
        EventMsg::ApplyPatchApprovalRequest(event) => {
            panic!("unexpected patch approval request: {:?}", event.call_id)
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_test_env_can_connect_and_use_filesystem() -> Result<()> {
    skip_if_no_remote_env!(Ok(()));

    let test_env = test_env().await?;
    let file_system = test_env.environment().get_filesystem();

    let file_path_uri = test_env.selection().cwd.join("remote-test-env-ok")?;
    let payload = b"remote-test-env-ok".to_vec();

    file_system
        .write_file(&file_path_uri, payload.clone(), /*sandbox*/ None)
        .await?;
    let actual = file_system
        .read_file(&file_path_uri, /*sandbox*/ None)
        .await?;
    assert_eq!(actual, payload);

    file_system
        .remove(
            &file_path_uri,
            RemoveOptions {
                recursive: false,
                force: true,
            },
            /*sandbox*/ None,
        )
        .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_test_env_exposes_target_shell_to_model() -> Result<()> {
    skip_if_no_remote_env!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.submit_turn("report remote environment").await?;

    let request = response_mock.single_request();
    let environment_context = request
        .message_input_texts("user")
        .into_iter()
        .find(|text| text.starts_with("<environment_context>"))
        .context("environment context should be model visible")?;
    // TODO(anp): Assert Wine-exec exposes a `C:\\...` cwd after model-visible paths preserve
    // target-native spelling instead of the Linux orchestrator's `/C:/...` representation.
    let expected_shell = match test_target_os() {
        TestTargetOs::Linux => "<shell>bash</shell>",
        TestTargetOs::Windows => "<shell>powershell</shell>",
        TestTargetOs::MacOs => unreachable!("remote test targets do not run macOS"),
    };
    assert_eq!(
        environment_context
            .lines()
            .find(|line| line.trim_start().starts_with("<shell>"))
            .map(str::trim),
        Some(expected_shell),
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_remote_shell_uses_snapshot_in_remote_cwd() -> Result<()> {
    const CALL_ID: &str = "remote-explicit-shell";

    skip_if_no_remote_env!(Ok(()));

    let (shell, command) = match test_target_os() {
        TestTargetOs::Linux => (
            "bash",
            r#"case "$PWD" in /tmp/codex-core-test-cwd-*) ;; *) echo "unexpected cwd: $PWD" >&2; exit 1 ;; esac; set -- "$PWD"/.codex-shell-snapshots/*.sh; test -f "$1"; grep -q '# Codex shell snapshot format: 3' "$1""#,
        ),
        TestTargetOs::Windows => (
            "powershell",
            r#"$cwd = (Get-Location).Path; if ($cwd -notlike 'C:\codex-core-test-cwd-*') { Write-Error "unexpected cwd: $cwd"; exit 1 }; $snapshot = Get-ChildItem -LiteralPath (Join-Path $cwd '.codex-shell-snapshots') -Filter '*.ps1' | Select-Object -First 1; if ($null -eq $snapshot) { Write-Error 'remote snapshot missing'; exit 1 }; if (-not (Select-String -LiteralPath $snapshot.FullName -SimpleMatch '# Codex PowerShell snapshot format: 1' -Quiet)) { Write-Error 'remote snapshot header missing'; exit 1 }"#,
        ),
        TestTargetOs::MacOs => unreachable!("remote test targets do not run macOS"),
    };

    let server = start_mock_server().await;
    let arguments = serde_json::to_string(&json!({
        "cmd": command,
        "shell": shell,
        "login": false,
        "yield_time_ms": 10_000,
    }))?;
    let mut builder = test_codex().with_config(|config| {
        config.use_experimental_unified_exec_tool = true;
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::ShellSnapshot)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(CALL_ID, "exec_command", &arguments),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_environments(
        "run the remote shell in the remote cwd",
        Some(vec![TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&test.config.cwd),
        }]),
    )
    .await?;
    let request = response_mock
        .last_request()
        .context("model should receive the command output")?;
    let (output, success) = request
        .function_call_output_content_and_success(CALL_ID)
        .context("remote shell tool result should be present")?;
    assert_ne!(success, Some(false));
    assert!(
        output.is_some_and(|output| output.contains("Process exited with code 0")),
        "remote shell command should exit successfully",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_does_not_duplicate_initial_environment_context() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        assert!(config.features.enable(Feature::DeferredExecutor).is_ok());
    });
    let test = builder.build(&server).await?;

    test.submit_turn("report the environment").await?;

    let user_context = response_mock.single_request().message_input_texts("user");
    assert_eq!(
        user_context
            .iter()
            .filter(|text| text.contains("<environment_context>"))
            .count(),
        1
    );

    Ok(())
}

async fn read_exec_server_json(websocket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        match timeout(Duration::from_secs(5), websocket.next())
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

async fn accept_initialized_exec_server(listener: TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("connection");
    let mut websocket = accept_async(stream).await.expect("websocket handshake");

    let initialize = read_exec_server_json(&mut websocket).await;
    assert_eq!(initialize["method"], "initialize");
    websocket
        .send(Message::Text(
            json!({
                "id": initialize["id"],
                "result": { "sessionId": "test-session" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("initialize response");
    let initialized = read_exec_server_json(&mut websocket).await;
    assert_eq!(initialized["method"], "initialized");

    websocket
}

async fn send_environment_info(websocket: &mut WebSocketStream<TcpStream>) {
    let info = read_exec_server_json(websocket).await;
    assert_eq!(info["method"], "environment/info");
    websocket
        .send(Message::Text(
            json!({
                "id": info["id"],
                "result": { "shell": { "name": "zsh", "path": "/bin/zsh" } }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("environment info response");
}

async fn serve_environment_info(listener: TcpListener) {
    let mut websocket = accept_initialized_exec_server(listener).await;
    send_environment_info(&mut websocket).await;
}

async fn serve_environment_with_agents_md(
    listener: TcpListener,
    contents: &str,
    attach: tokio::sync::oneshot::Receiver<()>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> usize {
    let mut websocket = accept_initialized_exec_server(listener).await;
    attach.await.expect("attach signal");
    send_environment_info(&mut websocket).await;

    let mut agents_md_reads = 0;
    let mut agents_md_handle_id = None;
    loop {
        let request = tokio::select! {
            request = read_exec_server_json(&mut websocket) => request,
            _ = &mut shutdown => return agents_md_reads,
        };
        let is_agents_md = request["params"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("/AGENTS.md"));
        let is_agents_md_handle = agents_md_handle_id
            .as_deref()
            .is_some_and(|handle_id| request["params"]["handleId"].as_str() == Some(handle_id));
        let response = match request["method"].as_str() {
            Some("fs/getMetadata") if is_agents_md => {
                json!({
                    "id": request["id"],
                    "result": {
                        "isDirectory": false,
                        "isFile": true,
                        "isSymlink": false,
                        "size": contents.len(),
                        "createdAtMs": 0,
                        "modifiedAtMs": 0,
                    }
                })
            }
            Some("fs/getMetadata") => json!({
                "id": request["id"],
                "error": { "code": -32004, "message": "not found" }
            }),
            Some("fs/open") if is_agents_md => {
                let handle_id = request["params"]["handleId"]
                    .as_str()
                    .expect("fs/open should include handleId")
                    .to_string();
                agents_md_handle_id = Some(handle_id.clone());
                json!({
                    "id": request["id"],
                    "result": { "handleId": handle_id }
                })
            }
            Some("fs/readBlock") if is_agents_md_handle => {
                agents_md_reads += 1;
                json!({
                    "id": request["id"],
                    "result": {
                        "chunk": BASE64_STANDARD.encode(contents),
                        "eof": true,
                    }
                })
            }
            Some("fs/close") if is_agents_md_handle => {
                agents_md_handle_id = None;
                json!({
                    "id": request["id"],
                    "result": {}
                })
            }
            method => panic!("unexpected exec-server request: {method:?}"),
        };
        websocket
            .send(Message::Text(response.to_string().into()))
            .await
            .expect("filesystem response");
    }
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

async fn wait_for_response_request_count(response_mock: &ResponseMock, expected_count: usize) {
    timeout(Duration::from_secs(15), async {
        while response_mock.requests().len() < expected_count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for Responses API request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_updates_context_and_tools_after_startup() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let wait_call_id = "wait-for-startup";
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    wait_call_id,
                    "wait_for_environment",
                    &json!({
                        "environment_id": REMOTE_ENVIRONMENT_ID,
                    })
                    .to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call(
                    "request-permissions",
                    "request_permissions",
                    &json!({
                        "reason": "Verify that the ready environment is used.",
                        "permissions": {
                            "network": { "enabled": true }
                        }
                    })
                    .to_string(),
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_config(|config| {
            config.project_doc_max_bytes = 0;
            config.use_experimental_unified_exec_tool = true;
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
            config.approvals_reviewer = ApprovalsReviewer::User;
            assert!(config.features.enable(Feature::DeferredExecutor).is_ok());
            assert!(config.features.enable(Feature::UnifiedExec).is_ok());
            assert!(
                config
                    .features
                    .enable(Feature::RequestPermissionsTool)
                    .is_ok()
            );
        });
    let test = timeout(Duration::from_secs(5), builder.build(&server))
        .await
        .context("DeferredExecutor session startup must not wait for the remote environment")??;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "wait for the environment".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 1).await;
    assert_eq!(response_mock.requests().len(), 1);
    serve_environment_info(listener).await;
    let event = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::RequestPermissions(_) | EventMsg::TurnComplete(_)
        )
    })
    .await;
    let EventMsg::RequestPermissions(permission_request) = event else {
        panic!("ready environment should be available to request_permissions: {event:?}");
    };
    assert_eq!(
        permission_request.environment_id.as_deref(),
        Some(REMOTE_ENVIRONMENT_ID)
    );
    test.codex
        .submit(Op::RequestPermissionsResponse {
            id: permission_request.call_id,
            response: RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let starting_tools = tool_names(&requests[0].body_json());
    let ready_tools = tool_names(&requests[1].body_json());
    assert!(starting_tools.contains(&"wait_for_environment".to_string()));
    assert!(!starting_tools.contains(&"exec_command".to_string()));
    assert!(ready_tools.contains(&"exec_command".to_string()));
    assert!(!ready_tools.contains(&"wait_for_environment".to_string()));
    let (wait_output, _) = requests[1]
        .function_call_output_content_and_success(wait_call_id)
        .context("wait_for_environment output should be present")?;
    assert_eq!(
        serde_json::from_str::<Value>(&wait_output.context("wait output should contain text")?)?,
        json!({
            "environment_id": REMOTE_ENVIRONMENT_ID,
            "status": "ready",
        })
    );
    assert!(
        requests[0]
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("<status>starting</status>"))
    );
    let ready_user_context = requests[1].message_input_texts("user");
    assert_eq!(
        ready_user_context
            .iter()
            .filter(|text| text.contains("<shell>zsh</shell>"))
            .count(),
        1
    );
    let final_user_context = requests[2].message_input_texts("user");
    assert_eq!(
        final_user_context
            .iter()
            .filter(|text| text.contains("<status>starting</status>"))
            .count(),
        0
    );
    assert_eq!(
        final_user_context
            .iter()
            .filter(|text| text.contains("<shell>zsh</shell>"))
            .count(),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_loads_agents_md_when_environment_becomes_ready() -> Result<()> {
    const AGENTS_CONTENT: &str = "REMOTE_AGENTS_INSTRUCTIONS";

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "wait-1",
                    "wait_for_environment",
                    &json!({ "environment_id": REMOTE_ENVIRONMENT_ID }).to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call(
                    "wait-2",
                    "wait_for_environment",
                    &json!({ "environment_id": REMOTE_ENVIRONMENT_ID }).to_string(),
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_config(|config| {
            assert!(config.features.enable(Feature::DeferredExecutor).is_ok());
        });
    let (attach_tx, attach_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let exec_server = tokio::spawn(serve_environment_with_agents_md(
        listener,
        AGENTS_CONTENT,
        attach_rx,
        shutdown_rx,
    ));
    let test = timeout(Duration::from_secs(5), builder.build(&server))
        .await
        .context("thread startup should not wait for the remote environment")??;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "load the environment instructions".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 1).await;
    let agents_path = PathUri::from_abs_path(&test.config.cwd).join("AGENTS.md")?;
    attach_tx.send(()).expect("attach environment");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    shutdown_tx.send(()).expect("stop exec server");
    let agents_md_reads = exec_server.await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        agents_md_reads >= 1,
        "ready environment should read AGENTS.md at least once"
    );
    assert_eq!(agents_md_occurrences(&requests[0], AGENTS_CONTENT), 0);
    assert_eq!(agents_md_occurrences(&requests[1], AGENTS_CONTENT), 1);
    assert_eq!(agents_md_occurrences(&requests[2], AGENTS_CONTENT), 1);
    assert_eq!(test.codex.instruction_sources().await, vec![agents_path]);

    Ok(())
}

fn agents_md_occurrences(request: &ResponsesRequest, contents: &str) -> usize {
    request
        .message_input_texts("user")
        .iter()
        .filter(|text| text.contains(contents))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_wait_reports_startup_failure() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let wait_call_id = "wait-for-failure";
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    wait_call_id,
                    "wait_for_environment",
                    &json!({
                        "environment_id": REMOTE_ENVIRONMENT_ID,
                    })
                    .to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_config(|config| {
            config.use_experimental_unified_exec_tool = true;
            assert!(config.features.enable(Feature::DeferredExecutor).is_ok());
            assert!(config.features.enable(Feature::UnifiedExec).is_ok());
        });
    let test = timeout(Duration::from_secs(5), builder.build(&server))
        .await
        .context("thread startup should not wait for the remote environment")??;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "wait for the environment".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 1).await;
    assert_eq!(response_mock.requests().len(), 1);
    let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .context("exec-server connection should arrive")??;
    drop(stream);
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let starting_tools = tool_names(&requests[0].body_json());
    let failed_tools = tool_names(&requests[1].body_json());
    assert!(starting_tools.contains(&"wait_for_environment".to_string()));
    assert!(!starting_tools.contains(&"exec_command".to_string()));
    assert!(!failed_tools.contains(&"wait_for_environment".to_string()));
    assert!(!failed_tools.contains(&"exec_command".to_string()));
    let (wait_output, _) = requests[1]
        .function_call_output_content_and_success(wait_call_id)
        .context("wait_for_environment output should be present")?;
    assert_eq!(
        wait_output.as_deref(),
        Some("Environment `remote` failed to start and is unavailable. Continue without it.")
    );
    assert!(
        requests[1]
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("status=\"unavailable\""))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_compaction_replaces_stale_environment_context() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "wait-for-startup",
                    "request_user_input",
                    &json!({
                        "questions": [{
                            "id": "continue",
                            "header": "Continue",
                            "question": "Continue after startup?",
                            "options": [{
                                "label": "Yes (Recommended)",
                                "description": "Continue the test."
                            }, {
                                "label": "No",
                                "description": "Stop the test."
                            }]
                        }]
                    })
                    .to_string(),
                ),
                ev_completed_with_tokens("resp-1", /*total_tokens*/ 96_000),
            ]),
            sse(vec![
                ev_assistant_message("msg-compact", "AUTO_COMPACT_SUMMARY"),
                ev_completed_with_tokens("resp-compact", /*total_tokens*/ 10_000),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_config(|config| {
            config.project_doc_max_bytes = 0;
            assert!(config.features.enable(Feature::DeferredExecutor).is_ok());
            assert!(
                config
                    .features
                    .enable(Feature::DefaultModeRequestUserInput)
                    .is_ok()
            );
            config.model_provider.name = "OpenAI (test)".to_string();
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config.model_context_window = Some(100_000);
            config.model_auto_compact_token_limit = Some(90_000);
        });
    let test = timeout(Duration::from_secs(5), builder.build(&server))
        .await
        .context("thread startup should not wait for the remote environment")??;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "wait for the environment".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;

    serve_environment_info(listener).await;
    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "continue".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["Yes (Recommended)".to_string()],
                    },
                )]),
                interrupted: false,
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let initial_context = requests[0].message_input_texts("user");
    assert!(
        initial_context
            .iter()
            .any(|text| text.contains("<status>starting</status>"))
    );

    let post_compaction_context = requests[2].message_input_texts("user");
    assert_eq!(
        post_compaction_context
            .iter()
            .filter(|text| text.contains("<status>starting</status>"))
            .count(),
        0
    );
    assert_eq!(
        post_compaction_context
            .iter()
            .filter(|text| text.contains("<shell>zsh</shell>"))
            .count(),
        1
    );
    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;
    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    let rollout = fs::read_to_string(rollout_path)?;
    let world_state_items = rollout
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::WorldState(item) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        world_state_items
            .iter()
            .map(|item| item.full)
            .collect::<Vec<_>>(),
        vec![true, true]
    );
    assert_eq!(
        world_state_items[0]
            .state
            .pointer("/environments/environments/remote/status"),
        Some(&json!("starting"))
    );
    assert_eq!(
        world_state_items[1]
            .state
            .pointer("/environments/environments/remote/status"),
        Some(&json!("available"))
    );
    assert_eq!(
        world_state_items[1]
            .state
            .pointer("/environments/environments/remote/shell"),
        Some(&json!("zsh"))
    );

    Ok(())
}

#[allow(dead_code)]
fn absolute_path(path: PathBuf) -> AbsolutePathBuf {
    AbsolutePathBuf::try_from(path).expect("path should be absolute")
}

#[allow(dead_code)]
fn read_only_sandbox(readable_root: PathBuf) -> FileSystemSandboxContext {
    let readable_root = absolute_path(readable_root);
    FileSystemSandboxContext::from_permission_profile(PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: readable_root,
            },
            access: FileSystemAccessMode::Read,
        }]),
        NetworkSandboxPolicy::Restricted,
    ))
}

#[allow(dead_code)]
fn workspace_write_sandbox(writable_root: PathBuf) -> FileSystemSandboxContext {
    let writable_root = absolute_path(writable_root);
    FileSystemSandboxContext::from_permission_profile(PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: writable_root,
            },
            access: FileSystemAccessMode::Write,
        }]),
        NetworkSandboxPolicy::Restricted,
    ))
}

#[allow(dead_code)]
fn assert_normalized_path_rejected(error: &std::io::Error) {
    match error.kind() {
        std::io::ErrorKind::NotFound => assert!(
            error.to_string().contains("No such file or directory"),
            "unexpected not-found message: {error}",
        ),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied => {
            let message = error.to_string();
            assert!(
                message.contains("is not permitted")
                    || message.contains("Operation not permitted")
                    || message.contains("Permission denied"),
                "unexpected rejection message: {message}",
            );
        }
        other => panic!("unexpected normalized-path error kind: {other:?}: {error:?}"),
    }
}

#[allow(dead_code)]
fn remote_exec(script: &str) -> Result<()> {
    let container_name = test_docker_container_name()
        .context("test requires direct access to the Docker container")?;
    let output = Command::new("docker")
        .args(["exec", container_name.as_str(), "sh", "-lc", script])
        .output()?;
    assert!(
        output.status.success(),
        "remote exec failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    );
    Ok(())
}

#[allow(dead_code)]
async fn exec_command_routing_output(
    test: &TestCodex,
    server: &wiremock::MockServer,
    call_id: &str,
    arguments: Value,
    environments: Option<Vec<TurnEnvironmentSelection>>,
) -> Result<String> {
    let response_mock = mount_sse_sequence(
        server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, "exec_command", &serde_json::to_string(&arguments)?),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_environments("route exec command", environments)
        .await?;

    response_mock
        .function_call_output_text(call_id)
        .with_context(|| format!("missing function_call_output for {call_id}"))
}
