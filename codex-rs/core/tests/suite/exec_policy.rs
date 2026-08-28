#![allow(clippy::unwrap_used)]

use anyhow::Result;
use codex_features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecPolicyAmendment;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::fs;

fn collaboration_mode_for_model(model: String) -> CollaborationMode {
    CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model,
            reasoning_effort: None,
            developer_instructions: Some("exercise approvals in collaboration mode".to_string()),
        },
    }
}

async fn submit_user_turn(
    test: &core_test_support::test_codex::TestCodex,
    prompt: &str,
    approval_policy: AskForApproval,
    permission_profile: PermissionProfile,
    collaboration_mode: Option<CollaborationMode>,
) -> Result<()> {
    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(permission_profile, test.config.cwd.as_path());
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
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(approval_policy),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: collaboration_mode.or({
                    Some(codex_protocol::config_types::CollaborationMode {
                        mode: codex_protocol::config_types::ModeKind::Default,
                        settings: codex_protocol::config_types::Settings {
                            model: session_model,
                            reasoning_effort: None,
                            developer_instructions: None,
                        },
                    })
                }),
                ..Default::default()
            },
        })
        .await?;
    Ok(())
}

fn assert_no_matched_rules_invariant(output_item: &Value) {
    let output = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("function call output should include a string output payload");
    assert!(
        !output.contains("invariant failed: matched_rules must be non-empty"),
        "unexpected invariant panic surfaced in output: {output}"
    );
}

#[tokio::test]
async fn unified_exec_disabled_windows_sandbox_rejects_managed_read_only_command() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
        config
            .features
            .disable(Feature::WindowsSandbox)
            .expect("test config should allow feature update");
        config
            .features
            .disable(Feature::WindowsSandboxElevated)
            .expect("test config should allow feature update");
        config.set_windows_sandbox_enabled(false);
        config.set_windows_elevated_sandbox_enabled(false);
    });
    let test = builder.build(&server).await?;
    let call_id = "unified-exec-disabled-windows-sandbox-read-only";
    let args = json!({
        "cmd": "cmd.exe /c dir",
        "yield_time_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-disabled-windows-sandbox-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-disabled-windows-sandbox-1"),
        ]),
    )
    .await;
    let results_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-disabled-windows-sandbox-1", "done"),
            ev_completed("resp-disabled-windows-sandbox-2"),
        ]),
    )
    .await;

    submit_user_turn(
        &test,
        "run unified exec with disabled Windows sandbox",
        AskForApproval::Never,
        PermissionProfile::read_only(),
        None,
    )
    .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output_item = results_mock.single_request().function_call_output(call_id);
    let output = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("function call output should include a string output payload");
    assert!(
        output.contains("cmd.exe /c dir") && output.contains("rejected: blocked by policy"),
        "unexpected output: {output}",
    );

    Ok(())
}

#[tokio::test]
async fn execpolicy_blocks_shell_invocation() -> Result<()> {
    let mut builder = test_codex().with_pre_build_hook(|codex_home| {
        let policy_path = codex_home.join("rules").join("policy.rules");
        fs::create_dir_all(
            policy_path
                .parent()
                .expect("policy directory must have a parent"),
        )
        .expect("create policy directory");
        fs::write(
            &policy_path,
            r#"prefix_rule(pattern=["echo"], decision="forbidden")"#,
        )
        .expect("write policy file");
    });
    let server = start_mock_server().await;
    let test = builder.build(&server).await?;

    let call_id = "shell-forbidden";
    let args = json!({
        "kind": "argv",
        "program": "echo",
        "args": ["blocked"],
        "timeout_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let results_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "run shell command".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output_item = results_mock.single_request().function_call_output(call_id);
    let output = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("function call output should include a string output payload");
    assert!(
        output.contains("policy forbids commands starting with `echo`"),
        "unexpected output: {output}",
    );

    Ok(())
}

async fn assert_direct_argv_shell_name_is_opaque(unified_exec: bool) -> Result<()> {
    let mut builder = test_codex()
        .with_pre_build_hook(|codex_home| {
            let policy_path = codex_home.join("rules").join("policy.rules");
            fs::create_dir_all(
                policy_path
                    .parent()
                    .expect("policy directory must have a parent"),
            )
            .expect("create policy directory");
            fs::write(
                &policy_path,
                r#"prefix_rule(pattern=["ls"], decision="allow")"#,
            )
            .expect("write policy file");
        })
        .with_config(move |config| {
            let result = if unified_exec {
                config.features.enable(Feature::UnifiedExec)
            } else {
                config.features.disable(Feature::UnifiedExec)
            };
            result.expect("test config should allow feature update");
        });
    let server = start_mock_server().await;
    let test = builder.build(&server).await?;
    let call_id = if unified_exec {
        "unified-direct-argv-shell-name"
    } else {
        "legacy-direct-argv-shell-name"
    };
    let tool_name = if unified_exec {
        "exec_command"
    } else {
        "shell_command"
    };
    let program = test
        .config
        .cwd
        .join("bash.exe")
        .to_string_lossy()
        .into_owned();
    let command = vec![program.clone(), "-lc".to_string(), "ls".to_string()];
    let args = if unified_exec {
        json!({
            "kind": "argv",
            "program": program,
            "args": ["-lc", "ls"],
            "yield_time_ms": 1_000,
        })
    } else {
        json!({
            "kind": "argv",
            "program": program,
            "args": ["-lc", "ls"],
            "timeout_ms": 1_000,
        })
    };

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-direct-argv-1"),
            ev_function_call(call_id, tool_name, &serde_json::to_string(&args)?),
            ev_completed("resp-direct-argv-1"),
        ]),
    )
    .await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-direct-argv-1", "done"),
            ev_completed("resp-direct-argv-2"),
        ]),
    )
    .await;

    submit_user_turn(
        &test,
        "run a direct argv command whose executable resembles a shell",
        AskForApproval::UnlessTrusted,
        PermissionProfile::Disabled,
        None,
    )
    .await?;

    let approval_event = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::ExecApprovalRequest(_) | EventMsg::Error(_) | EventMsg::TurnComplete(_)
        )
    })
    .await;
    let approval = match approval_event {
        EventMsg::ExecApprovalRequest(approval) => approval,
        other => panic!("expected approval for exact direct argv command, got {other:?}"),
    };
    assert_eq!(approval.command, command);
    assert_eq!(
        approval.proposed_execpolicy_amendment,
        Some(ExecPolicyAmendment::new(command))
    );

    test.codex
        .submit(Op::ExecApproval {
            id: approval.effective_approval_id(),
            turn_id: None,
            decision: ReviewDecision::Denied,
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

fn run_direct_argv_shell_name_is_opaque(unified_exec: bool) -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    let test_name = if unified_exec {
        "authorization_identity_unified_direct_argv_shell_name_is_opaque"
    } else {
        "authorization_identity_legacy_direct_argv_shell_name_is_opaque"
    };
    let handle = std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(Box::pin(assert_direct_argv_shell_name_is_opaque(
                unified_exec,
            )))
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[test]
fn authorization_identity_legacy_direct_argv_shell_name_is_opaque() -> Result<()> {
    run_direct_argv_shell_name_is_opaque(false)
}

#[test]
fn authorization_identity_unified_direct_argv_shell_name_is_opaque() -> Result<()> {
    run_direct_argv_shell_name_is_opaque(true)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_empty_script_with_collaboration_mode_does_not_panic() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let test = builder.build(&server).await?;
    let call_id = "shell-empty-script-collab";
    let args = json!({
        "command": "",
        "timeout_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-empty-shell-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-empty-shell-1"),
        ]),
    )
    .await;
    let results_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-empty-shell-1", "done"),
            ev_completed("resp-empty-shell-2"),
        ]),
    )
    .await;

    let collaboration_mode = collaboration_mode_for_model(test.session_configured.model.clone());
    submit_user_turn(
        &test,
        "run an empty shell command",
        AskForApproval::OnRequest,
        PermissionProfile::Disabled,
        Some(collaboration_mode),
    )
    .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output_item = results_mock.single_request().function_call_output(call_id);
    assert_no_matched_rules_invariant(&output_item);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_empty_script_with_collaboration_mode_does_not_panic() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build(&server).await?;
    let call_id = "unified-exec-empty-script-collab";
    let args = json!({
        "cmd": "",
        "yield_time_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-empty-unified-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-empty-unified-1"),
        ]),
    )
    .await;
    let results_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-empty-unified-1", "done"),
            ev_completed("resp-empty-unified-2"),
        ]),
    )
    .await;

    let collaboration_mode = collaboration_mode_for_model(test.session_configured.model.clone());
    submit_user_turn(
        &test,
        "run empty unified exec command",
        AskForApproval::OnRequest,
        PermissionProfile::Disabled,
        Some(collaboration_mode),
    )
    .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output_item = results_mock.single_request().function_call_output(call_id);
    assert_no_matched_rules_invariant(&output_item);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_whitespace_script_with_collaboration_mode_does_not_panic() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2");
    let test = builder.build(&server).await?;
    let call_id = "shell-whitespace-script-collab";
    let args = json!({
        "command": "  \n\t  ",
        "timeout_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-whitespace-shell-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-whitespace-shell-1"),
        ]),
    )
    .await;
    let results_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-whitespace-shell-1", "done"),
            ev_completed("resp-whitespace-shell-2"),
        ]),
    )
    .await;

    let collaboration_mode = collaboration_mode_for_model(test.session_configured.model.clone());
    submit_user_turn(
        &test,
        "run whitespace shell command",
        AskForApproval::OnRequest,
        PermissionProfile::Disabled,
        Some(collaboration_mode),
    )
    .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output_item = results_mock.single_request().function_call_output(call_id);
    assert_no_matched_rules_invariant(&output_item);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_whitespace_script_with_collaboration_mode_does_not_panic() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build(&server).await?;
    let call_id = "unified-exec-whitespace-script-collab";
    let args = json!({
        "cmd": " \n \t",
        "yield_time_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-whitespace-unified-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-whitespace-unified-1"),
        ]),
    )
    .await;
    let results_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-whitespace-unified-1", "done"),
            ev_completed("resp-whitespace-unified-2"),
        ]),
    )
    .await;

    let collaboration_mode = collaboration_mode_for_model(test.session_configured.model.clone());
    submit_user_turn(
        &test,
        "run whitespace unified exec command",
        AskForApproval::OnRequest,
        PermissionProfile::Disabled,
        Some(collaboration_mode),
    )
    .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output_item = results_mock.single_request().function_call_output(call_id);
    assert_no_matched_rules_invariant(&output_item);

    Ok(())
}
