use super::*;
use codex_apply_patch::MaybeApplyPatchVerified;
use codex_exec_server::LOCAL_FS;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::FileChange;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolInvocation;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::turn_diff_tracker::TurnDiffTracker;

fn sample_patch() -> &'static str {
    r#"*** Begin Patch
*** Add File: hello.txt
+hello
*** End Patch"#
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_remote_apply_patch_preserves_requested_sandbox() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use codex_protocol::config_types::WindowsSandboxLevel;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::models::ResponseInputItem;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSpecialPath;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ReviewDecision;
    use futures::SinkExt;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    for (sandbox_enabled, deny_write) in [(true, false), (true, true), (false, false)] {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let backing = TempDir::new().unwrap();
        let remote_file = backing.path().join("remote.txt");
        std::fs::write(&remote_file, b"original\n").unwrap();
        let cwd = workspace.path().abs();
        let cwd_uri = PathUri::from_abs_path(&cwd);
        let target_uri = cwd_uri.join("remote.txt").unwrap();
        let permissions = if sandbox_enabled {
            PermissionProfile::from_runtime_permissions(
                &FileSystemSandboxPolicy::restricted(vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::Root,
                        },
                        access: FileSystemAccessMode::Read,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::project_roots(None),
                        },
                        access: FileSystemAccessMode::Write,
                    },
                ]),
                NetworkSandboxPolicy::Restricted,
            )
        } else {
            PermissionProfile::Disabled
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let server_file = remote_file.clone();
        let server_target = target_uri.clone();
        let server_cwd = cwd_uri.clone();
        let observed_operations = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server_operations = Arc::clone(&observed_operations);
        let server = tokio::spawn(async move {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                _ = &mut stop_rx => return Vec::new(),
            };
            let mut socket = tokio_tungstenite::accept_async(accepted.0).await.unwrap();
            let mut writes = Vec::new();
            loop {
                let frame = tokio::select! {
                    frame = socket.next() => frame,
                    _ = &mut stop_rx => break,
                };
                let Some(Ok(frame)) = frame else { break };
                let message: serde_json::Value = match frame {
                    Message::Text(text) => serde_json::from_str(text.as_ref()).unwrap(),
                    Message::Binary(bytes) => serde_json::from_slice(bytes.as_ref()).unwrap(),
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    other => panic!("unexpected executor frame: {other:?}"),
                };
                let method = message["method"].as_str().unwrap();
                server_operations.lock().unwrap().push(method.to_string());
                let mut error = None;
                let result = match method {
                    "initialize" => json!({"sessionId": "remote-patch-sandbox"}),
                    "initialized" => continue,
                    "environment/info" => json!({
                        "operatingSystem": "windows",
                        "shell": {"name": "cmd", "path": "cmd.exe"},
                        "cwd": server_cwd,
                    }),
                    "fs/canonicalize" => json!({"path": message["params"]["path"]}),
                    "fs/getMetadata" => json!({
                        "isDirectory": false, "isFile": true, "isSymlink": false,
                        "size": std::fs::metadata(&server_file).unwrap().len(),
                    }),
                    "fs/readFile" => json!({
                        "dataBase64": STANDARD.encode(std::fs::read(&server_file).unwrap()),
                    }),
                    "fs/writeFile" => {
                        let params = message["params"].clone();
                        assert_eq!(params["path"], json!(server_target));
                        writes.push(params.clone());
                        if deny_write {
                            error = Some(
                                json!({"code": -32000, "message": "Permission denied: remote patch policy"}),
                            );
                        } else {
                            let bytes = STANDARD
                                .decode(params["dataBase64"].as_str().unwrap())
                                .unwrap();
                            std::fs::write(&server_file, bytes).unwrap();
                        }
                        json!({})
                    }
                    method => panic!("unexpected executor operation {method}: {message}"),
                };
                let response = match error {
                    Some(error) => json!({"id": message["id"], "error": error}),
                    None => json!({"id": message["id"], "result": result}),
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
            writes
        });
        let (session, mut turn, events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("Test API Key"),
                Vec::new(),
                home.path(),
                |config| {
                    config.cwd = cwd.clone();
                    config.workspace_roots = vec![cwd.clone()];
                    config.permissions.approval_policy =
                        crate::config::Constrained::allow_any(AskForApproval::UnlessTrusted);
                    config.permissions.windows_sandbox_private_desktop = deny_write;
                    config
                        .permissions
                        .set_permission_profile(permissions.clone())
                        .unwrap();
                },
            )
            .await;
        let turn_mut = Arc::get_mut(&mut turn).unwrap();
        // On Windows this makes select_initial return None while policy still
        // requests sandboxing. Other hosts prove the same portable wire policy.
        turn_mut.windows_sandbox_level = WindowsSandboxLevel::Disabled;
        turn_mut.model_info.apply_patch_tool_type =
            Some(codex_protocol::openai_models::ApplyPatchToolType::Freeform);
        turn_mut.environments.turn_environments = vec![TurnEnvironment::new(
            "patch-remote".into(),
            Arc::new(codex_exec_server::Environment::create_for_tests(Some(url)).unwrap()),
            cwd_uri.clone(),
            None,
        )];
        *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
        let step = StepContext::for_test(turn);
        let router = Arc::new(crate::tools::router::ToolRouter::from_context(
            step.as_ref(),
            crate::tools::router::ToolRouterParams {
                tool_suggest_candidates: None,
                deferred_mcp_tools: None,
                mcp_tools: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
                exposure_identity: Default::default(),
            },
            &Default::default(),
        ));
        assert!(step.set_tool_router(router).is_ok());
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            session.clone(),
            step,
            Arc::new(Mutex::new(TurnDiffTracker::new())),
        );
        let call = runtime.handle_tool_call(
            crate::tools::router::ToolCall {
                tool_name: codex_tools::ToolName::plain("apply_patch"),
                call_id: "remote-sandbox-patch".into(),
                payload: ToolPayload::Custom {
                    input: "*** Begin Patch\n*** Add File: remote.txt\n+replacement\n*** End Patch"
                        .into(),
                },
            },
            tokio_util::sync::CancellationToken::new(),
        );
        tokio::pin!(call);
        let mut approvals = 0;
        let response = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                tokio::select! {
                    result = &mut call => break result.unwrap(),
                    event = events.recv() => {
                        if let EventMsg::ApplyPatchApprovalRequest(request) = event.unwrap().msg {
                            approvals += 1;
                            let decision = if approvals == 1 { ReviewDecision::Approved } else { ReviewDecision::Denied };
                            session.notify_approval(&request.call_id, decision).await;
                        }
                    }
                }
            }
        }).await.unwrap_or_else(|error| {
            panic!("registered remote patch completes (sandbox={sandbox_enabled}, deny={deny_write}, approvals={approvals}, operations={:?}): {error}", observed_operations.lock().unwrap())
        });
        stop_tx.send(()).unwrap();
        let writes = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            writes.len(),
            1,
            "one remote write attempt, no unapproved retry"
        );
        assert!(
            approvals >= 1,
            "registered patch must enter approval orchestration"
        );
        assert_eq!(writes[0]["path"], json!(target_uri));
        assert_eq!(
            STANDARD
                .decode(writes[0]["dataBase64"].as_str().unwrap())
                .unwrap(),
            b"replacement\n"
        );
        let expected_sandbox =
            sandbox_enabled.then(|| codex_exec_server::FileSystemSandboxContext {
                permissions: permissions.into(),
                cwd: Some(cwd_uri),
                workspace_roots: Vec::new(),
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_private_desktop: deny_write,
            });
        assert_eq!(
            writes[0]["sandbox"],
            json!(expected_sandbox),
            "remote write must retain canonical policy intent without host workspace roots"
        );
        let ResponseInputItem::CustomToolCallOutput { output, .. } = response else {
            panic!("registered patch must return a custom tool output");
        };
        let text = output.body.to_text().unwrap();
        if deny_write {
            assert!(!text.contains("Success. Updated"), "{text}");
            assert!(
                text.contains("Exit code: 1") && text.contains("Failed to write file"),
                "{text}"
            );
            assert_eq!(
                std::fs::read(&remote_file).unwrap(),
                b"original\n",
                "denied patch must preserve remote contents"
            );
        } else {
            assert!(text.contains("Success. Updated"), "{text}");
            assert_eq!(std::fs::read(&remote_file).unwrap(), b"replacement\n");
        }
        assert!(
            !cwd.join("remote.txt").exists(),
            "selected remote patch must never write through the host filesystem"
        );
    }
}

async fn invocation_for_payload(payload: ToolPayload) -> ToolInvocation {
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "call-apply-patch".to_string(),
        tool_name: codex_tools::ToolName::plain("apply_patch"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload,
    }
}

#[tokio::test]
async fn pre_tool_use_payload_uses_freeform_patch_input() {
    let patch = sample_patch();
    let payload = ToolPayload::Custom {
        input: patch.to_string(),
    };
    let invocation = invocation_for_payload(payload).await;
    let handler = ApplyPatchHandler::default();

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_input: json!({ "command": patch }),
        })
    );
}

#[tokio::test]
async fn foreign_patch_uses_only_an_external_sandbox_profile() {
    let (_session, mut turn) = make_session_and_context().await;
    let file_paths = vec![PathUri::parse("file:///tmp/remote.txt").expect("POSIX path URI")];

    assert!(external_patch_permissions(&turn, file_paths.clone()).is_none());

    turn.permission_profile = PermissionProfile::External {
        network: NetworkSandboxPolicy::Restricted,
    };
    let (returned_paths, permissions, policy) =
        external_patch_permissions(&turn, file_paths.clone()).expect("external sandbox profile");

    assert_eq!(returned_paths, file_paths);
    assert_eq!(permissions.additional_permissions, None);
    assert!(!permissions.permissions_preapproved);
    assert_eq!(policy, FileSystemSandboxPolicy::external_sandbox());
}

#[tokio::test]
async fn post_tool_use_payload_uses_patch_input_and_tool_output() {
    let patch = sample_patch();
    let payload = ToolPayload::Custom {
        input: patch.to_string(),
    };
    let invocation = invocation_for_payload(payload).await;
    let output = ApplyPatchToolOutput::from_text("Success. Updated files.".to_string());
    let handler = ApplyPatchHandler::default();

    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(PostToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_use_id: "call-apply-patch".to_string(),
            tool_input: json!({ "command": patch }),
            tool_response: json!("Success. Updated files."),
        })
    );
}

#[test]
fn diff_consumer_streams_apply_patch_changes() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta("call-1".to_string(), "*** Begin Patch\n")
            .is_none()
    );

    let event = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("progress event");
    assert_eq!(
        (event.call_id, event.changes),
        (
            "call-1".to_string(),
            HashMap::from([(
                PathBuf::from("hello.txt"),
                FileChange::Add {
                    content: String::new(),
                },
            )]),
        )
    );

    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n+world")
            .is_none()
    );
    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n*** End Patch")
            .is_none()
    );

    let event = consumer
        .finish_update_on_complete()
        .expect("finish parser")
        .expect("progress event");
    assert_eq!(
        (event.call_id, event.changes),
        (
            "call-1".to_string(),
            HashMap::from([(
                PathBuf::from("hello.txt"),
                FileChange::Add {
                    content: "hello\nworld\n".to_string(),
                },
            )]),
        )
    );
}

#[test]
fn diff_consumer_streams_apply_patch_changes_with_environment_header() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta(
                "call-1".to_string(),
                "*** Begin Patch\n*** Environment ID: remote\n",
            )
            .is_none()
    );

    let event = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("progress event");
    assert_eq!(
        event.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: String::new(),
            },
        )])
    );
}

#[test]
fn diff_consumer_sends_next_update_after_buffer_interval() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    consumer.push_delta("call-1".to_string(), "*** Begin Patch\n");
    let first = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("first progress event");
    assert_eq!(
        first.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: String::new(),
            },
        )])
    );

    consumer.last_sent_at =
        Some(std::time::Instant::now() - APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL);
    let second = consumer
        .push_delta("call-1".to_string(), "\n+world")
        .expect("second progress event");
    assert_eq!(
        second.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: "hello\n".to_string(),
            },
        )])
    );
}

#[test]
fn diff_consumer_permanently_suppresses_progress_after_parse_error() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta("call-1".to_string(), "*** Begin Patch\n")
            .is_none()
    );
    assert!(
        consumer
            .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello",)
            .is_some()
    );
    consumer.last_sent_at = Some(std::time::Instant::now());
    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n+world")
            .is_none()
    );
    assert!(consumer.pending.is_some());

    assert!(
        consumer
            .push_delta("call-1".to_string(), "\ninvalid line\n")
            .is_none()
    );
    assert!(consumer.pending.is_none());

    consumer.last_sent_at =
        Some(std::time::Instant::now() - APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL);
    assert!(
        consumer
            .push_delta(
                "call-1".to_string(),
                "*** Add File: later.txt\n+later\n*** Update File: hello.txt\n@@\n-hello\n+goodbye\n*** End Patch",
            )
            .is_none()
    );
    assert!(consumer.pending.is_none());
    let Err(FunctionCallError::RespondToModel(message)) = consumer.finish_update_on_complete()
    else {
        panic!("expected the original streaming parse failure");
    };
    assert_eq!(
        message,
        "failed to parse apply_patch: invalid hunk at line 5, 'invalid line' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'"
    );
}

#[test]
fn reconcile_environment_id_requires_selection_when_enabled() {
    assert_eq!(
        require_environment_id(Some("remote"), /*allow_environment_id*/ false),
        Err(FunctionCallError::RespondToModel(
            "apply_patch environment selection is unavailable for this turn".to_string(),
        ))
    );
    assert_eq!(
        require_environment_id(
            /*parsed_environment_id*/ None, /*allow_environment_id*/ true
        ),
        Ok(None)
    );
}

#[tokio::test]
async fn input_state_determined_environment_mismatch_blocks_exact_retry_without_mutation() {
    let (session, mut turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_environment = turn
        .environments
        .primary()
        .expect("primary environment")
        .clone();
    let selected_environment_id = turn_environment.environment_id.clone();
    let cwd = turn_environment.cwd().clone();
    let selected_target_path = cwd
        .to_abs_path()
        .expect("local environment cwd")
        .join("phase31-environment-mismatch.txt");
    let other_environment_dir = TempDir::new().expect("other environment cwd");
    let other_environment_cwd = PathUri::from_abs_path(&other_environment_dir.path().abs());
    let other_environment = TurnEnvironment::new(
        "phase31-environment-b".to_string(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        other_environment_cwd.clone(),
        /*shell*/ None,
    );
    let patch_environment_id = other_environment.environment_id.clone();
    let other_target_path = other_environment_cwd
        .to_abs_path()
        .expect("other local environment cwd")
        .join("phase31-environment-mismatch.txt");
    assert_ne!(
        selected_target_path.parent(),
        other_target_path.parent(),
        "test environments must have distinct cwds"
    );
    turn.environments.turn_environments.push(other_environment);
    let patch = format!(
        "*** Begin Patch\n*** Environment ID: {patch_environment_id}\n*** Add File: phase31-environment-mismatch.txt\n+must not be written\n*** End Patch"
    );
    let command = vec!["apply_patch".to_string(), patch];
    let fs = turn_environment.environment.get_filesystem();
    let attempt_key =
        CommandAttemptKey::new("shell", &selected_environment_id, cwd.to_string(), &command);
    session
        .services
        .command_execution
        .begin_attempt(&attempt_key, false)
        .await
        .expect("first attempt");

    let result = intercept_apply_patch(
        /*is_validation*/ false,
        &command,
        &cwd,
        fs.as_ref(),
        turn_environment,
        session.clone(),
        Arc::new(turn),
        /*tracker*/ None,
        "phase31-call",
        "shell",
        tokio_util::sync::CancellationToken::new(),
    )
    .await;

    let Err(error) = result else {
        panic!("expected an environment mismatch error");
    };
    error
        .record_attempt_failure(&session.services.command_execution, &attempt_key)
        .await;
    let FunctionCallError::RespondToModel(message) = error.into_error() else {
        panic!("expected a model-visible environment mismatch error");
    };
    assert_eq!(
        message,
        format!(
            "apply_patch verification failed: patch environment id `{patch_environment_id}` does not match selected shell environment `{selected_environment_id}`"
        )
    );
    assert!(
        !selected_target_path.exists(),
        "mismatched patch must not mutate selected environment A"
    );
    assert!(
        !other_target_path.exists(),
        "mismatched patch must not redirect mutation to environment B"
    );
    let blocked = session
        .services
        .command_execution
        .begin_attempt(&attempt_key, false)
        .await
        .expect_err("the unchanged environment mismatch must be suppressed");
    assert!(
        blocked
            .render_for_model()
            .contains("apply_patch environment mismatch")
    );
}

#[tokio::test]
async fn input_state_determined_implicit_patch_blocks_exact_retry_without_mutation() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let turn_environment = turn
        .environments
        .primary()
        .expect("primary environment")
        .clone();
    let cwd = turn_environment.cwd().clone();
    let target_path = cwd
        .to_abs_path()
        .expect("local environment cwd")
        .join("phase-input-determined-implicit.txt");
    let command = vec![
        "*** Begin Patch\n*** Add File: phase-input-determined-implicit.txt\n+must not be written\n*** End Patch"
            .to_string(),
    ];
    let attempt_key = CommandAttemptKey::new(
        "shell",
        &turn_environment.environment_id,
        cwd.to_string(),
        &command,
    );
    session
        .services
        .command_execution
        .begin_attempt(&attempt_key, false)
        .await
        .expect("first attempt");

    let error = match intercept_apply_patch(
        /*is_validation*/ false,
        &command,
        &cwd,
        turn_environment.environment.get_filesystem().as_ref(),
        turn_environment,
        Arc::clone(&session),
        Arc::clone(&turn),
        /*tracker*/ None,
        "implicit-patch-call",
        "shell",
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("a raw patch body must require an explicit apply_patch invocation"),
    };
    error
        .record_attempt_failure(&session.services.command_execution, &attempt_key)
        .await;
    let first_message = error.into_error().to_string();
    assert!(first_message.contains("explicit call to apply_patch"));
    assert!(
        !target_path.exists(),
        "implicit patch must not mutate the workspace"
    );

    let blocked = session
        .services
        .command_execution
        .begin_attempt(&attempt_key, false)
        .await
        .expect_err("the unchanged implicit invocation must be suppressed");
    assert!(
        blocked
            .render_for_model()
            .contains("apply_patch implicit invocation")
    );
}

#[tokio::test]
async fn validation_commands_bypass_apply_patch_interception() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let turn_environment = turn
        .environments
        .primary()
        .expect("primary environment")
        .clone();
    let cwd = turn_environment.cwd().clone();
    let command = vec![
        "apply_patch".to_string(),
        "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch".to_string(),
    ];

    let result = intercept_apply_patch(
        /*is_validation*/ true,
        &command,
        &cwd,
        turn_environment.environment.get_filesystem().as_ref(),
        turn_environment,
        session,
        turn,
        /*tracker*/ None,
        "validation-apply-patch-call",
        "shell",
        tokio_util::sync::CancellationToken::new(),
    )
    .await;

    assert!(
        matches!(result, Ok(None)),
        "validation must proceed through ordinary command execution"
    );
}

#[tokio::test]
async fn approval_keys_include_move_destination() {
    let tmp = TempDir::new().expect("tmp");
    let cwd_path = tmp.path();
    let cwd = cwd_path.abs();
    std::fs::create_dir_all(cwd_path.join("old")).expect("create old dir");
    std::fs::create_dir_all(cwd_path.join("renamed/dir")).expect("create dest dir");
    std::fs::write(cwd_path.join("old/name.txt"), "old content\n").expect("write old file");
    let patch = r#"*** Begin Patch
*** Update File: old/name.txt
*** Move to: renamed/dir/name.txt
@@
-old content
+new content
*** End Patch"#;
    let argv = vec!["apply_patch".to_string(), patch.to_string()];
    // TODO(anp): Keep apply_patch handler test cwd values as PathUri.
    let cwd = PathUri::from_abs_path(&cwd);
    let action = match codex_apply_patch::maybe_parse_apply_patch_verified(
        &argv,
        &cwd,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    {
        MaybeApplyPatchVerified::Body(action) => action,
        other => panic!("expected patch body, got: {other:?}"),
    };

    let keys = file_paths_for_action(&action);
    assert_eq!(keys.len(), 2);
}

#[test]
fn write_permissions_for_paths_skip_dirs_already_writable_under_workspace_root() {
    let tmp = TempDir::new().expect("tmp");
    let cwd_path = tmp.path();
    let cwd = cwd_path.abs();
    let nested = cwd_path.join("nested");
    std::fs::create_dir_all(&nested).expect("create nested dir");
    let file_path = AbsolutePathBuf::try_from(nested.join("file.txt"))
        .expect("nested file path should be absolute");
    let sandbox_policy = FileSystemSandboxPolicy::workspace_write(
        &[],
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ false,
    );

    let permissions = write_permissions_for_paths(&[file_path], &sandbox_policy, &cwd);

    assert_eq!(permissions, None);
}

#[test]
fn write_permissions_for_paths_keep_dirs_outside_workspace_root() {
    let tmp = TempDir::new().expect("tmp");
    let cwd = tmp.path().join("workspace");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    std::fs::create_dir_all(&outside).expect("create outside dir");
    let file_path = AbsolutePathBuf::try_from(outside.join("file.txt"))
        .expect("outside file path should be absolute");
    let cwd_abs = cwd.abs();
    let sandbox_policy = FileSystemSandboxPolicy::workspace_write(
        &[],
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );

    let permissions = write_permissions_for_paths(&[file_path], &sandbox_policy, &cwd_abs);
    let expected_outside =
        dunce::simplified(&outside.canonicalize().expect("canonicalize outside dir")).abs();

    assert_eq!(
        permissions
            .and_then(|profile| profile.file_system)
            .and_then(|fs| fs.legacy_read_write_roots())
            .and_then(|(_read, write)| write),
        Some(vec![expected_outside])
    );
}
