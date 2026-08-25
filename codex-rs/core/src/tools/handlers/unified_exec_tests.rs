use super::exec_command::attach_powershell_failure_advisory;
use super::exec_command::validate_and_consume_remote_shell;
use super::*;
use crate::shell::ShellType;
use crate::shell::default_user_shell;
use codex_exec_server::Environment;
use codex_git_utils::get_git_repo_root;
use codex_protocol::models::PermissionProfile;
use codex_tools::ToolExecutor;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;

use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::turn_diff_tracker::TurnDiffTracker;
use tokio::sync::Mutex;

const TEST_TRUNCATION_POLICY: TruncationPolicy = TruncationPolicy::Tokens(10_000);

async fn run_exec_command_for_test(
    session: &Arc<crate::session::session::Session>,
    turn: &Arc<crate::session::turn_context::TurnContext>,
    call_id: &str,
    payload: ToolPayload,
) -> Box<dyn ToolOutput> {
    ExecCommandHandler::default()
        .handle(ToolInvocation {
            session: Arc::clone(session),
            step_context: StepContext::for_test(Arc::clone(turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: call_id.to_string(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
            source: ToolCallSource::Direct,
            payload,
        })
        .await
        .expect("exec_command test invocation succeeds")
}

async fn wait_for_exec_command_end(
    rx_event: &async_channel::Receiver<codex_protocol::protocol::Event>,
    call_id: &str,
) -> (bool, bool) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut begin_has_process_id = None;
        loop {
            let event = rx_event
                .recv()
                .await
                .expect("session event channel remains open");
            match event.msg {
                codex_protocol::protocol::EventMsg::ExecCommandBegin(event)
                    if event.call_id == call_id =>
                {
                    begin_has_process_id = Some(event.process_id.is_some());
                }
                codex_protocol::protocol::EventMsg::ExecCommandEnd(event)
                    if event.call_id == call_id =>
                {
                    break (
                        begin_has_process_id.expect("exec command begin event arrives before end"),
                        event.process_id.is_some(),
                    );
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("exec command end event arrives for {call_id}"))
}

#[test]
fn terminal_powershell_failure_keeps_recovery_advisory_out_of_raw_output() {
    let raw_output = b"ParserError: Unexpected token 'foo'".to_vec();
    let existing_repair_notice = "Preflight repaired the command.";
    let mut output = ExecCommandToolOutput {
        event_call_id: "call-parser-failure".to_string(),
        chunk_id: "chunk-parser-failure".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: raw_output.clone(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(1),
        original_token_count: None,
        hook_command: Some("broken command".to_string()),
        raw_output_artifact: None,
        repair_notice: Some(existing_repair_notice.to_string()),
    };

    attach_powershell_failure_advisory(
        &mut output,
        ShellType::PowerShell,
        /*is_powershell_script*/ false,
    );

    assert_eq!(output.raw_output, raw_output);
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let canonical = output
        .canonical_result(&payload)
        .expect("shell output has canonical bytes");
    assert_eq!(canonical.bytes, raw_output);
    assert_eq!(canonical.exact_bytes, raw_output.len() as u64);
    let projection = output
        .projection_metadata()
        .expect("shell output has one bounded typed projection");
    assert_eq!(projection.spillable_text.len(), 1);
    assert!(projection.fragments.iter().any(|fragment| {
        fragment.kind == codex_tools::ToolOutputProjectionFragmentKind::ProcessFinalStatus
    }));
    let repair_notice = output
        .repair_notice
        .as_deref()
        .expect("PowerShell failure should expose model recovery guidance");
    assert!(repair_notice.starts_with(existing_repair_notice));
    assert!(repair_notice.contains("retry with `kind: \"powershell_script\"`"));
    assert!(projection.fragments.iter().any(|fragment| {
        fragment.kind == codex_tools::ToolOutputProjectionFragmentKind::ErrorOrDiagnostic
            && fragment.text == repair_notice
    }));
    assert_eq!(projection.essential_inline["repair_notice"], repair_notice);
    assert_eq!(projection.essential_inline["wall_time_seconds"], 0.01);

    assert_eq!(
        output.post_tool_use_response("call-parser-failure", &payload),
        Some(serde_json::json!("ParserError: Unexpected token 'foo'"))
    );
    let code_mode = output.code_mode_result(&payload);
    assert_eq!(code_mode["repair"], repair_notice);
    assert!(
        !code_mode["output"]
            .as_str()
            .expect("code-mode output should be text")
            .contains("retry with `kind: \"powershell_script\"`")
    );
}

async fn invocation_for_payload(
    tool_name: &str,
    call_id: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain(tool_name),
        source: ToolCallSource::Direct,
        payload,
    }
}

async fn invocation_for_payload_without_sandbox(
    tool_name: &str,
    call_id: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    let turn = Arc::new(turn);

    ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain(tool_name),
        source: ToolCallSource::Direct,
        payload,
    }
}

async fn invocation_for_payload_with_shellless_remote(
    call_id: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    let turn_environment = turn
        .environments
        .turn_environments
        .first_mut()
        .expect("primary test environment");
    turn_environment.environment_id = "shellless-remote".to_string();
    turn_environment.environment = Arc::new(
        Environment::create_for_tests(Some(
            "ws://127.0.0.1:1/phase79-shellless-remote".to_string(),
        ))
        .expect("remote test environment"),
    );
    turn_environment.shell = None;
    let turn = Arc::new(turn);

    ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: ToolCallSource::Direct,
        payload,
    }
}

#[test]
fn test_get_command_uses_default_shell_when_unspecified() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert!(args.shell.is_none());

    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ true,
        /*environment_is_remote*/ false,
    )
    .map_err(anyhow::Error::msg)?;
    let command = resolved.command;

    assert_eq!(command.len(), 3);
    assert_eq!(command[2], "echo hello");
    Ok(())
}

#[test]
fn test_get_command_launches_structured_argv_without_shell_wrapping() -> anyhow::Result<()> {
    let args: ExecCommandArgs =
        parse_arguments(r#"{"kind":"argv","program":"rg","args":["--files"]}"#)?;

    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ false,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(
        resolved.command,
        vec!["rg".to_string(), "--files".to_string()]
    );
    assert_eq!(resolved.safety_command, resolved.command);
    assert_eq!(resolved.preflight_shell_type, None);
    Ok(())
}

#[tokio::test]
async fn repeated_rg_miss_uses_workspace_identity_across_epoch_advance() {
    let (mut session, turn) = make_session_and_context().await;
    let workspace_cwd = turn
        .environments
        .single_local_environment_cwd()
        .expect("test turn has one local environment");
    session.services.command_execution =
        crate::tools::command_execution::CommandExecutionLedger::load_or_new(
            turn.config.codex_home.to_path_buf(),
            session.thread_id.to_string(),
            workspace_cwd.as_path(),
        )
        .await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let repo_root =
        get_git_repo_root(workspace_cwd.as_path()).expect("test cwd is in a git repository");
    let search_target = repo_root.join("codex-rs/core/src/tools/command_execution.rs");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "rg",
            "args": [
                "-n",
                "__codex_negative_cache_unmatched_probe__",
                search_target,
            ],
        })
        .to_string(),
    };
    let second_tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    second_tracker.lock().await.record_unknown_mutation();

    let ((first, second), launches) =
        crate::tools::runtimes::unified_exec::test_observation::observe(async {
            let first = run_exec_command_for_test(
                &session,
                &turn,
                "negative-cache-first-miss",
                payload.clone(),
            )
            .await;
            let second = ExecCommandHandler::default()
                .handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: tokio_util::sync::CancellationToken::new(),
                    tracker: Arc::clone(&second_tracker),
                    call_id: "negative-cache-repeated-miss".to_string(),
                    tool_name: codex_tools::ToolName::plain("exec_command"),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await;
            (first, second)
        })
        .await;

    assert_eq!(launches.process_launches, 1);
    assert_eq!(first.code_mode_result(&payload)["exit_code"], 1);
    let second_error = match second {
        Ok(_) => panic!("the equivalent negative search should be suppressed"),
        Err(error) => error,
    };
    let message = second_error.to_string();
    assert!(message.contains("equivalent search already produced a negative result"));
    assert!(message.contains("execution was suppressed"));
}

#[tokio::test]
async fn rg_miss_in_alternate_repository_is_invalidated_after_mutation() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let alternate_repository = tempfile::tempdir().expect("create alternate repository");
    let git_init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(alternate_repository.path())
        .output()
        .expect("initialize alternate repository");
    assert!(git_init.status.success());
    let search_target = alternate_repository.path().join("search-target.txt");
    tokio::fs::write(&search_target, "before\n")
        .await
        .expect("write initial search target");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "rg",
            "args": ["-n", "after", search_target],
            "workdir": alternate_repository.path(),
        })
        .to_string(),
    };

    let ((first, second), launches) =
        crate::tools::runtimes::unified_exec::test_observation::observe(async {
            let first = run_exec_command_for_test(
                &session,
                &turn,
                "alternate-repository-first-miss",
                payload.clone(),
            )
            .await;
            tokio::fs::write(&search_target, "after\n")
                .await
                .expect("mutate alternate repository search target");
            let second_tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
            second_tracker.lock().await.record_unknown_mutation();
            let second = ExecCommandHandler::default()
                .handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: StepContext::for_test(Arc::clone(&turn)),
                    cancellation_token: tokio_util::sync::CancellationToken::new(),
                    tracker: second_tracker,
                    call_id: "alternate-repository-after-mutation".to_string(),
                    tool_name: codex_tools::ToolName::plain("exec_command"),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .expect("mutated alternate-repository search should execute");
            (first, second)
        })
        .await;

    assert_eq!(launches.process_launches, 2);
    assert_eq!(first.code_mode_result(&payload)["exit_code"], 1);
    let second_result = second.code_mode_result(&payload);
    assert_eq!(second_result["exit_code"], 0);
    assert!(
        second_result["output"]
            .as_str()
            .is_some_and(|output| output.contains("after"))
    );
}

#[tokio::test]
async fn known_delta_unified_exec_reuses_third_exact_git_show_and_force_fresh_launches() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    assert!(
        session
            .features()
            .enabled(codex_features::Feature::KnownDeltaStore)
    );
    let repo_root =
        get_git_repo_root(turn.cwd().as_path()).expect("test cwd is in a git repository");
    let blob_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD:codex-rs/core/src/task_evidence.rs"])
        .current_dir(&repo_root)
        .output()
        .expect("resolve committed test blob");
    assert!(
        blob_output.status.success(),
        "git rev-parse failed: {}",
        String::from_utf8_lossy(&blob_output.stderr)
    );
    let blob = String::from_utf8(blob_output.stdout)
        .expect("blob id is UTF-8")
        .trim()
        .to_string();
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "git",
            "args": ["show", blob.clone()],
            "yield_time_ms": 10_000,
        })
        .to_string(),
    };

    let ((first, second, third, cached_lifecycle), launches) =
        crate::tools::runtimes::unified_exec::test_observation::observe(
            crate::tools::known_delta_store::test_observation::with_profitability_costs(
                async {
                    let first = run_exec_command_for_test(
                        &session,
                        &turn,
                        "known-delta-unified-first",
                        payload.clone(),
                    )
                    .await;
                    wait_for_exec_command_end(&rx_event, "known-delta-unified-first").await;
                    let second = run_exec_command_for_test(
                        &session,
                        &turn,
                        "known-delta-unified-second",
                        payload.clone(),
                    )
                    .await;
                    wait_for_exec_command_end(&rx_event, "known-delta-unified-second").await;
                    let third = run_exec_command_for_test(
                        &session,
                        &turn,
                        "known-delta-unified-third",
                        payload.clone(),
                    )
                    .await;
                    let cached_lifecycle =
                        wait_for_exec_command_end(&rx_event, "known-delta-unified-third").await;
                    (first, second, third, cached_lifecycle)
                },
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(1),
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    assert_eq!(launches.process_launches, 2);
    assert_eq!(cached_lifecycle, (false, false));

    let canonical_text = |output: &dyn ToolOutput| {
        String::from_utf8(
            output
                .canonical_result(&payload)
                .expect("exec output has canonical bytes")
                .bytes,
        )
        .expect("git show output is UTF-8")
    };
    assert!(!canonical_text(first.as_ref()).contains("known-delta cache hit"));
    assert!(!canonical_text(second.as_ref()).contains("known-delta cache hit"));
    assert!(canonical_text(third.as_ref()).contains("known-delta cache hit"));
    let third_code_mode = third.code_mode_result(&payload);
    assert_eq!(third_code_mode["exit_code"], 0);
    assert!(third_code_mode.get("session_id").is_none());
    let second_artifact_id = second.code_mode_result(&payload)["raw_output_artifact_id"]
        .as_str()
        .expect("shadow validation has an output artifact")
        .to_string();
    let third_artifact_id = third_code_mode["raw_output_artifact_id"]
        .as_str()
        .expect("cache hit has a reminted output artifact");
    assert_ne!(third_artifact_id, second_artifact_id);

    let force_fresh_payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "git",
            "args": ["show", blob],
            "yield_time_ms": 10_000,
            "force_fresh": true,
        })
        .to_string(),
    };
    let (fresh, fresh_launches) = crate::tools::runtimes::unified_exec::test_observation::observe(
        crate::tools::known_delta_store::test_observation::with_profitability_costs(
            run_exec_command_for_test(
                &session,
                &turn,
                "known-delta-unified-force-fresh",
                force_fresh_payload.clone(),
            ),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_secs(1),
        ),
    )
    .await;
    wait_for_exec_command_end(&rx_event, "known-delta-unified-force-fresh").await;
    assert_eq!(fresh_launches.process_launches, 1);
    let fresh_text = String::from_utf8(
        fresh
            .canonical_result(&force_fresh_payload)
            .expect("fresh exec output has canonical bytes")
            .bytes,
    )
    .expect("fresh git show output is UTF-8");
    assert!(!fresh_text.contains("known-delta cache hit"));

    let (reused_after_fresh, post_fresh_launches) =
        crate::tools::runtimes::unified_exec::test_observation::observe(
            crate::tools::known_delta_store::test_observation::with_profitability_costs(
                run_exec_command_for_test(
                    &session,
                    &turn,
                    "known-delta-unified-after-fresh",
                    payload.clone(),
                ),
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(1),
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    wait_for_exec_command_end(&rx_event, "known-delta-unified-after-fresh").await;
    assert_eq!(post_fresh_launches.process_launches, 0);
    assert!(canonical_text(reused_after_fresh.as_ref()).contains("known-delta cache hit"));
}

#[test]
fn test_get_command_encodes_powershell_script_but_keeps_plain_safety_shape() -> anyhow::Result<()> {
    let args: ExecCommandArgs =
        parse_arguments(r#"{"kind":"powershell_script","script_body":"Get-ChildItem -Force"}"#)?;
    let powershell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh"),
    };

    let resolved = get_command(
        &args,
        Arc::new(powershell),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ false,
    )
    .map_err(anyhow::Error::msg)?;

    assert!(resolved.command.iter().any(|arg| arg == "-EncodedCommand"));
    assert!(resolved.safety_command.iter().any(|arg| arg == "-Command"));
    assert_eq!(
        resolved.safety_command.last().map(String::as_str),
        Some("Get-ChildItem -Force")
    );
    assert_eq!(resolved.preflight_shell_type, Some(ShellType::PowerShell));
    Ok(())
}

#[test]
fn test_get_command_rejects_powershell_script_for_non_powershell_remote() -> anyhow::Result<()> {
    let args: ExecCommandArgs =
        parse_arguments(r#"{"kind":"powershell_script","script_body":"Get-ChildItem"}"#)?;
    let bash = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let err = get_command(
        &args,
        Arc::new(bash),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ true,
    )
    .expect_err("remote shell mismatch should be rejected");
    assert!(err.contains("remote environment to report PowerShell"));
    Ok(())
}

#[test]
fn accepted_remote_shell_uses_the_remote_reported_path() -> anyhow::Result<()> {
    let remote_shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/remote-only-phase89/bin/bash"),
    };
    let mut args: ExecCommandArgs = parse_arguments(
        r#"{"kind":"script","cmd":"printf remote","shell":"/remote-only-phase89/bin/bash"}"#,
    )?;

    validate_and_consume_remote_shell(&mut args, Some(&remote_shell), "remote-phase89")
        .map_err(anyhow::Error::msg)?;
    assert!(args.shell.is_none());

    let resolved = get_command(
        &args,
        Arc::new(remote_shell.clone()),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ true,
    )
    .map_err(anyhow::Error::msg)?;
    assert_eq!(
        resolved.command.first().map(String::as_str),
        Some("/remote-only-phase89/bin/bash")
    );

    let mut mismatched: ExecCommandArgs = parse_arguments(
        r#"{"kind":"script","cmd":"printf remote","shell":"/remote-only-phase89/bin/pwsh"}"#,
    )?;
    let err =
        validate_and_consume_remote_shell(&mut mismatched, Some(&remote_shell), "remote-phase89")
            .expect_err("a different remote shell type must remain rejected");
    assert!(err.contains("only supports `bash`"));
    Ok(())
}

#[tokio::test]
async fn shellless_remote_handler_rejects_shell_commands_but_allows_argv() {
    let handler = ExecCommandHandler::default();
    let shell_commands = [
        (
            "shellless-remote-script",
            serde_json::json!({"kind": "script", "cmd": "printf remote"}),
        ),
        (
            "shellless-remote-powershell",
            serde_json::json!({
                "kind": "powershell_script",
                "script_body": "Get-ChildItem"
            }),
        ),
    ];

    for (call_id, arguments) in shell_commands {
        let invocation = invocation_for_payload_with_shellless_remote(
            call_id,
            ToolPayload::Function {
                arguments: arguments.to_string(),
            },
        )
        .await;
        let error = match handler.handle(invocation).await {
            Ok(_) => panic!("shell-wrapped remote commands require reported shell metadata"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "environment `shellless-remote` does not report a shell"
        );
    }

    let argv_invocation = invocation_for_payload_with_shellless_remote(
        "shellless-remote-argv",
        ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": "git",
                "args": ["--worktree", "status"]
            })
            .to_string(),
        },
    )
    .await;
    let argv_error = match handler.handle(argv_invocation).await {
        Ok(_) => panic!("the intentionally invalid argv should fail during preflight"),
        Err(error) => error,
    };
    assert!(
        argv_error.to_string().contains("known_flag_typo"),
        "structured argv must pass the shell-metadata guard and reach preflight: {argv_error}"
    );

    let argv_with_shell_invocation = invocation_for_payload_with_shellless_remote(
        "shellless-remote-argv-with-shell",
        ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": "git",
                "args": ["status"],
                "shell": "bash"
            })
            .to_string(),
        },
    )
    .await;
    let argv_with_shell_error = match handler.handle(argv_with_shell_invocation).await {
        Ok(_) => panic!("structured argv must not accept a shell override"),
        Err(error) => error,
    };
    assert_eq!(
        argv_with_shell_error.to_string(),
        "`shell` is only valid for script commands; omit it when `kind` is `argv`."
    );
}

#[tokio::test]
async fn read_only_preflight_repair_executes_and_releases_process_id() {
    let invocation = invocation_for_payload(
        "exec_command",
        "preflight-repair",
        ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": "rg",
                "args": ["--ignorecase", "--version"],
                "yield_time_ms": 10_000
            })
            .to_string(),
        },
    )
    .await;
    let session = Arc::clone(&invocation.session);
    let handler = ExecCommandHandler::default();

    let output = handler
        .handle(invocation)
        .await
        .expect("read-only typo should be repaired and executed");
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert!(
        code_mode["repair"]
            .as_str()
            .is_some_and(|repair| repair.contains("known_flag_typo"))
    );
    assert!(code_mode["raw_output_artifact_id"].is_string());

    let process_id = session
        .services
        .unified_exec_manager
        .allocate_process_id()
        .await;
    assert_eq!(process_id, 1000);
    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
}

#[tokio::test]
async fn mutating_preflight_rejection_does_not_reserve_process_id() {
    let invocation = invocation_for_payload(
        "exec_command",
        "preflight-reject-mutating",
        ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": "git",
                "args": ["--worktree", "status"]
            })
            .to_string(),
        },
    )
    .await;
    let session = Arc::clone(&invocation.session);
    let handler = ExecCommandHandler::default();

    let err = match handler.handle(invocation).await {
        Ok(_) => panic!("mutating command typo must be rejected"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("known_flag_typo"));

    let process_id = session
        .services
        .unified_exec_manager
        .allocate_process_id()
        .await;
    assert_eq!(process_id, 1000);
    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
}

#[tokio::test]
async fn intercepted_apply_patch_failure_releases_process_id_and_remains_retryable() {
    let patch = "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch";
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "apply_patch",
            "args": [patch]
        })
        .to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let handler = ExecCommandHandler::default();

    for attempt in 0..2 {
        let err = match handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: format!("intercept-failure-{attempt}"),
                tool_name: codex_tools::ToolName::plain("exec_command"),
                source: ToolCallSource::Direct,
                payload: payload.clone(),
            })
            .await
        {
            Ok(_) => panic!("invalid intercepted patch must fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("apply_patch verification failed"));

        let process_id = session
            .services
            .unified_exec_manager
            .allocate_process_id()
            .await;
        assert_eq!(process_id, 1000);
        session
            .services
            .unified_exec_manager
            .release_process_id(process_id)
            .await;
    }

    let payload_with_output_only_change = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "apply_patch",
            "args": [patch],
            "max_output_tokens": 1
        })
        .to_string(),
    };
    let third_failure = match handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "intercept-failure-blocked".to_string(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
            source: ToolCallSource::Direct,
            payload: payload_with_output_only_change,
        })
        .await
    {
        Ok(_) => panic!("third identical failure must still fail verification"),
        Err(err) => err,
    };
    let third_failure = third_failure.to_string();
    assert!(third_failure.contains("apply_patch verification failed"));
    assert!(!third_failure.contains("execution was suppressed"));

    let artifact_directory = turn
        .config
        .codex_home
        .join("tool-output")
        .join(session.thread_id.to_string());
    assert!(
        !tokio::fs::try_exists(artifact_directory)
            .await
            .expect("inspect artifact directory")
    );
}

#[test]
fn test_get_command_rejects_non_windows_shell_override() -> anyhow::Result<()> {
    let args: ExecCommandArgs =
        parse_arguments(r#"{"kind":"script","cmd":"echo hello","shell":"bash"}"#)?;
    let powershell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh.exe"),
    };

    let err = get_command(
        &args,
        Arc::new(powershell),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ false,
    )
    .expect_err("non-Windows shell override must be rejected");
    assert!(err.contains("unsupported Windows shell"));
    Ok(())
}

#[tokio::test]
async fn repeated_apply_patch_environment_mismatch_is_suppressed_before_process_launch() {
    let (session, turn) = make_session_and_context().await;
    let selected_environment_id = turn
        .environments
        .primary()
        .expect("primary environment")
        .environment_id
        .clone();
    let patch_environment_id = format!("{selected_environment_id}-mismatch");
    let patch = format!(
        "*** Begin Patch\n*** Environment ID: {patch_environment_id}\n*** Add File: must-not-exist.txt\n+must not be written\n*** End Patch"
    );
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "apply_patch",
            "args": [patch]
        })
        .to_string(),
    };
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let handler = ExecCommandHandler::default();

    let invoke = |call_id: &str| ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    };

    let ((first_result, second_result), launches) =
        crate::tools::runtimes::unified_exec::test_observation::observe(async {
            let first = handler.handle(invoke("environment-mismatch-first")).await;
            let second = handler.handle(invoke("environment-mismatch-second")).await;
            (first, second)
        })
        .await;

    let first_error = match first_result {
        Ok(_) => panic!("the mismatched patch environment must fail verification"),
        Err(error) => error.to_string(),
    };
    assert!(first_error.contains("apply_patch verification failed"));
    assert!(first_error.contains("does not match selected shell environment"));
    assert!(!first_error.contains("execution was suppressed"));

    let second_error = match second_result {
        Ok(_) => panic!("the exact repeated environment mismatch must be suppressed"),
        Err(error) => error.to_string(),
    };
    assert!(second_error.contains("apply_patch environment mismatch"));
    assert!(second_error.contains("execution was suppressed"));
    assert_eq!(launches.process_launches, 0);

    let process_id = session
        .services
        .unified_exec_manager
        .allocate_process_id()
        .await;
    assert_eq!(process_id, 1000, "interception must not launch a process");
    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
}

#[tokio::test]
async fn intercepted_apply_patch_success_reports_terminal_completion_and_post_hook() {
    let temp_dir = tempfile::tempdir_in(std::env::current_dir().expect("current directory"))
        .expect("create apply_patch fixture directory");
    let target = "phase89-intercept.txt";
    let patch =
        format!("*** Begin Patch\n*** Update File: {target}\n@@\n-before\n+after\n*** End Patch");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "apply_patch",
            "args": [patch],
            "workdir": temp_dir.path(),
        })
        .to_string(),
    };
    let invocation = invocation_for_payload_without_sandbox(
        "exec_command",
        "intercept-success",
        payload.clone(),
    )
    .await;
    let target_path = temp_dir.path().join(target);
    tokio::fs::write(&target_path, "before\n")
        .await
        .expect("write apply_patch fixture");
    let handler = ExecCommandHandler::default();
    let pre_hook = handler
        .pre_tool_use_payload(&invocation)
        .expect("intercepted apply_patch should expose Bash PreToolUse");

    let output = handler
        .handle(invocation.clone())
        .await
        .expect("valid intercepted patch should succeed");
    let code_mode = output.code_mode_result(&payload);
    assert_eq!(code_mode["exit_code"], 0);
    assert!(
        code_mode["wall_time_seconds"]
            .as_f64()
            .is_some_and(|wall_time| wall_time > 0.0)
    );

    let post_hook = handler
        .post_tool_use_payload(&invocation, output.as_ref())
        .expect("successful interception should expose Bash PostToolUse");
    assert_eq!(post_hook.tool_name, HookToolName::exec_command());
    assert_eq!(post_hook.tool_input, pre_hook.tool_input);
    assert_eq!(post_hook.tool_use_id, "intercept-success");
    let patch_result = post_hook
        .tool_response
        .as_str()
        .expect("successful Bash PostToolUse should carry the patch result");
    assert!(patch_result.contains("Exit code: 0"));
    assert!(patch_result.contains(&format!("M {target}")));
    assert_eq!(
        tokio::fs::read_to_string(target_path)
            .await
            .expect("read patched fixture"),
        "after\n"
    );
}

#[tokio::test]
async fn kd4_latency_unpolled_background_failure_retires_live_metadata() {
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Python is required by the KD4 test environment");
    let script = "import time; print('X' * 5000, flush=True); time.sleep(2.5); print('BACKGROUND_FINAL_MARKER'); raise SystemExit(7)";
    let program = python.to_string_lossy().into_owned();
    let command = vec![program.clone(), "-c".to_string(), script.to_string()];
    let (session, turn) = make_session_and_context().await;
    tokio::fs::create_dir_all(turn.config.codex_home.as_path())
        .await
        .expect("create test codex home");
    session
        .services
        .exec_policy
        .append_amendment_and_update(
            turn.config.codex_home.as_path(),
            &codex_protocol::protocol::ExecPolicyAmendment::new(command.clone()),
        )
        .await
        .expect("allow the bounded background test command");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let artifact_directory = turn
        .config
        .codex_home
        .join("tool-output")
        .join(session.thread_id.to_string());
    let invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "background-finalization".to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": program,
                "args": ["-c", script],
                "yield_time_ms": 250
            })
            .to_string(),
        },
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ExecCommandHandler::default().handle(invocation),
    )
    .await
    .expect("background exec_command should yield within ten seconds")
    .expect("background command should start");
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    let process_id = code_mode["session_id"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .expect("numeric background process id");
    let running = session
        .services
        .command_execution
        .running_process(process_id)
        .await
        .expect("background process must be tracked while it is running");
    let attempt_key = running.key;
    let artifact_id = running
        .artifact
        .model_projection()
        .0
        .expect("background process should own a retained artifact");
    let mut retained = String::new();
    let mut consecutive_failures = 0;
    let mut running_metadata_retired = false;
    for _ in 0..100 {
        retained = tokio::fs::read_to_string(artifact_directory.join(format!("{artifact_id}.log")))
            .await
            .unwrap_or_default();
        running_metadata_retired = session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .is_none();
        consecutive_failures = session
            .services
            .command_execution
            .consecutive_failures(&attempt_key)
            .await;
        if retained.contains("BACKGROUND_FINAL_MARKER")
            && consecutive_failures == 1
            && running_metadata_retired
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(retained.contains("BACKGROUND_FINAL_MARKER"));
    assert_eq!(consecutive_failures, 1);
    assert!(running_metadata_retired);
}

#[tokio::test]
async fn foreground_output_artifact_retains_bytes_beyond_transcript_cap() {
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Python is required by the KD4 test environment");
    let segment_bytes = crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
    let script = format!(
        "import sys; sys.stdout.buffer.write(b'BEGIN\\n' + b'A' * {segment_bytes} + b'\\nMIDDLE_MARKER\\n' + b'B' * {segment_bytes} + b'\\nEND\\n'); sys.stdout.buffer.flush()"
    );
    let program = python.to_string_lossy().into_owned();
    let command = vec![program.clone(), "-c".to_string(), script.clone()];
    let (session, turn) = make_session_and_context().await;
    tokio::fs::create_dir_all(turn.config.codex_home.as_path())
        .await
        .expect("create test codex home");
    session
        .services
        .exec_policy
        .append_amendment_and_update(
            turn.config.codex_home.as_path(),
            &codex_protocol::protocol::ExecPolicyAmendment::new(command),
        )
        .await
        .expect("allow the bounded large-output test command");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let artifact_directory = turn
        .config
        .codex_home
        .join("tool-output")
        .join(session.thread_id.to_string());
    let invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "full-output-artifact".to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": program,
                "args": ["-c", script],
                "yield_time_ms": 20_000,
                "max_output_tokens": 2_000
            })
            .to_string(),
        },
    };

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(40),
        ExecCommandHandler::default().handle(invocation),
    )
    .await
    .expect("large-output exec_command should finish within forty seconds")
    .expect("large-output command should succeed");
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(code_mode["exit_code"], 0);
    assert!(code_mode.get("session_id").is_none());
    let artifact_id = code_mode["raw_output_artifact_id"]
        .as_str()
        .expect("raw output artifact id");
    let artifact_path = artifact_directory.join(format!("{artifact_id}.log"));
    let artifact = tokio::fs::read(&artifact_path)
        .await
        .expect("read raw output artifact");
    assert!(artifact.len() > segment_bytes * 2);
    assert!(artifact.starts_with(b"BEGIN"));
    assert!(
        artifact
            .windows(b"MIDDLE_MARKER".len())
            .any(|window| window == b"MIDDLE_MARKER")
    );
    assert!(artifact.ends_with(b"END\r\n") || artifact.ends_with(b"END\n"));
    assert_eq!(
        code_mode["raw_output_artifact_bytes"],
        artifact.len() as u64
    );
    let model_output = code_mode["output"].as_str().expect("model output");
    assert!(model_output.len() < segment_bytes);
    assert!(!model_output.contains("MIDDLE_MARKER"));
}

#[test]
#[cfg(not(windows))]
fn test_get_command_respects_explicit_bash_shell() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "/bin/bash"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert_eq!(args.shell.as_deref(), Some("/bin/bash"));

    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ true,
        /*environment_is_remote*/ false,
    )
    .map_err(anyhow::Error::msg)?;
    let command = resolved.command;

    assert_eq!(command.last(), Some(&"echo hello".to_string()));
    if command
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case("-Command"))
    {
        assert!(command.contains(&"-NoProfile".to_string()));
    }
    Ok(())
}

#[test]
fn test_get_command_respects_explicit_powershell_shell() -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let powershell_path = temp_dir.path().join("powershell.exe");
    std::fs::write(&powershell_path, "")?;
    let json = serde_json::json!({
        "cmd": "echo hello",
        "shell": powershell_path,
    })
    .to_string();

    let args: ExecCommandArgs = parse_arguments(&json)?;

    assert_eq!(
        args.shell.as_deref(),
        Some(powershell_path.to_string_lossy().as_ref())
    );

    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ true,
        /*environment_is_remote*/ false,
    )
    .map_err(anyhow::Error::msg)?;
    let command = resolved.command;

    assert_eq!(command[2], "echo hello");
    assert_eq!(resolved.shell_type, ShellType::PowerShell);
    Ok(())
}

#[test]
fn test_get_command_respects_explicit_cmd_shell() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "cmd"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert_eq!(args.shell.as_deref(), Some("cmd"));

    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ true,
        /*environment_is_remote*/ false,
    )
    .map_err(anyhow::Error::msg)?;
    let command = resolved.command;

    assert_eq!(command[2], "echo hello");
    Ok(())
}

#[test]
fn test_get_command_rejects_explicit_login_when_disallowed() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "login": true}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;
    let err = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ false,
    )
    .expect_err("explicit login should be rejected");

    assert!(
        err.contains("login shell is disabled by config"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn exec_command_pre_tool_use_payload_ignores_base_sensitive_permission_fields() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "cmd": "printf exec command",
            "additional_permissions": {
                "file_system": {
                    "write": ["relative-output"]
                }
            }
        })
        .to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let handler = ExecCommandHandler::default();
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "call-43".to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload,
    };

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(crate::tools::registry::PreToolUsePayload {
            tool_name: HookToolName::exec_command(),
            tool_input: serde_json::json!({ "command": "printf exec command" }),
        })
    );

    let rewritten = handler
        .with_updated_hook_input(
            invocation,
            serde_json::json!({ "command": "printf rewritten" }),
        )
        .expect("hook rewrite should not deserialize relative permission paths");
    let ToolPayload::Function { arguments } = rewritten.payload else {
        panic!("rewritten exec_command payload should remain function-shaped");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&arguments)
            .expect("rewritten exec_command arguments should remain valid JSON"),
        serde_json::json!({
            "cmd": "printf rewritten",
            "additional_permissions": {
                "file_system": {
                    "write": ["relative-output"]
                }
            }
        })
    );
}

#[tokio::test]
async fn exec_command_hook_preserves_and_rewrites_direct_argv_structurally() {
    let arguments = serde_json::json!({
        "kind": "argv",
        "program": "rg",
        "args": ["--files"],
        "timeout_ms": 1234
    })
    .to_string();
    let invocation = invocation_for_payload(
        "exec_command",
        "argv-hook-rewrite",
        ToolPayload::Function {
            arguments: arguments.clone(),
        },
    )
    .await;
    let handler = ExecCommandHandler::default();
    let updated_input = handler
        .pre_tool_use_payload(&invocation)
        .expect("argv invocation should expose hook input");
    assert_eq!(updated_input.tool_name, HookToolName::exec_command());
    assert_eq!(
        updated_input.tool_input,
        serde_json::json!({
            "command": "rg --files",
            "kind": "argv",
            "program": "rg",
            "args": ["--files"],
        })
    );

    let rewritten = handler
        .with_updated_hook_input(invocation.clone(), updated_input.tool_input)
        .expect("unchanged argv display should preserve structured invocation");
    let ToolPayload::Function {
        arguments: rewritten_arguments,
    } = rewritten.payload
    else {
        panic!("rewritten exec_command payload should remain function-shaped");
    };
    assert_eq!(rewritten_arguments, arguments);

    let args: ExecCommandArgs =
        parse_arguments(&rewritten_arguments).expect("preserved argv should still parse");
    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ false,
    )
    .expect("preserved argv should resolve directly");
    assert_eq!(resolved.command, vec!["rg", "--files"]);
    assert_eq!(resolved.preflight_shell_type, None);

    let rewritten = handler
        .with_updated_hook_input(
            invocation.clone(),
            serde_json::json!({
                "kind": "argv",
                "program": "kds",
                "args": [
                    "--agent",
                    "path with spaces",
                    "quote\"inside",
                    "",
                    "Grüße 世界",
                ],
            }),
        )
        .expect("structured argv rewrite should remain direct");
    let ToolPayload::Function { arguments } = rewritten.payload else {
        panic!("rewritten exec_command payload should remain function-shaped");
    };
    let args: ExecCommandArgs =
        parse_arguments(&arguments).expect("rewritten argv should still parse");
    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ false,
    )
    .expect("rewritten argv should resolve directly");
    assert_eq!(
        resolved.command,
        vec![
            "kds",
            "--agent",
            "path with spaces",
            "quote\"inside",
            "",
            "Grüße 世界",
        ]
    );
    assert_eq!(resolved.preflight_shell_type, None);

    let err = handler
        .with_updated_hook_input(
            invocation,
            serde_json::json!({ "command": "rg --files --hidden" }),
        )
        .err()
        .expect("changed argv display must not be downgraded to a script");
    assert!(
        err.to_string()
            .contains("would lose structured `program`/`args`"),
        "unexpected argv rewrite error: {err}"
    );
}

#[tokio::test]
async fn exec_command_pre_tool_use_payload_skips_write_stdin() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "chars": "echo hi" }).to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let handler = WriteStdinHandler;

    assert_eq!(
        handler.pre_tool_use_payload(&ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-44".to_string(),
            tool_name: codex_tools::ToolName::plain("write_stdin"),
            source: crate::tools::context::ToolCallSource::Direct,
            payload,
        }),
        None
    );
}

#[tokio::test]
async fn exec_command_post_tool_use_payload_uses_output_for_noninteractive_one_shot_commands() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "cmd": "echo three", "tty": false }).to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "call-43".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };
    let invocation = invocation_for_payload("exec_command", "call-43", payload).await;
    let handler = ExecCommandHandler::default();
    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::exec_command(),
            tool_use_id: "call-43".to_string(),
            tool_input: serde_json::json!({ "command": "echo three" }),
            tool_response: serde_json::json!("three"),
        })
    );
}

#[tokio::test]
async fn exec_command_post_tool_use_payload_uses_output_for_interactive_completion() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "cmd": "echo three", "tty": true }).to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "call-44".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };
    let invocation = invocation_for_payload("exec_command", "call-44", payload).await;
    let handler = ExecCommandHandler::default();

    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::exec_command(),
            tool_use_id: "call-44".to_string(),
            tool_input: serde_json::json!({ "command": "echo three" }),
            tool_response: serde_json::json!("three"),
        })
    );
}

#[tokio::test]
async fn exec_command_post_tool_use_payload_skips_running_sessions() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "cmd": "echo three", "tty": false }).to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "event-45".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: Some(45),
        exit_code: None,
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };
    let invocation = invocation_for_payload("exec_command", "call-45", payload).await;
    let handler = ExecCommandHandler::default();
    assert_eq!(handler.post_tool_use_payload(&invocation, &output), None);
}

#[tokio::test]
async fn write_stdin_post_tool_use_payload_uses_original_exec_call_id_and_command_on_completion() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "session_id": 45,
            "chars": "",
        })
        .to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "exec-call-45".to_string(),
        chunk_id: "chunk-2".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"finished\n".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("sleep 1; echo finished".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };
    let invocation = invocation_for_payload("write_stdin", "write-stdin-call", payload).await;
    let handler = WriteStdinHandler;

    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::exec_command(),
            tool_use_id: "exec-call-45".to_string(),
            tool_input: serde_json::json!({ "command": "sleep 1; echo finished" }),
            tool_response: serde_json::json!("finished\n"),
        })
    );
}

#[tokio::test]
async fn empty_write_stdin_poll_does_not_increment_retry_or_reentry_counters() {
    let invocation = invocation_for_payload(
        "write_stdin",
        "ordinary-poll",
        ToolPayload::Function {
            arguments: serde_json::json!({
                "session_id": u32::MAX,
                "chars": "",
                "yield_time_ms": 10,
            })
            .to_string(),
        },
    )
    .await;
    let timing = Arc::new(crate::tools::tool_dispatch_trace::ToolDispatchTiming::new(
        tokio::time::Instant::now(),
        false,
    ));
    let _ = crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing(
        Arc::clone(&timing),
        WriteStdinHandler.handle(invocation),
    )
    .await;

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(snapshot.retry_count, 0);
    assert_eq!(snapshot.reentry_count, 0);
}

#[tokio::test]
async fn write_stdin_post_tool_use_payload_keeps_parallel_session_metadata_separate() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "session_id": 45, "chars": "" }).to_string(),
    };
    let output_a = ExecCommandToolOutput {
        event_call_id: "exec-call-a".to_string(),
        chunk_id: "chunk-a".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"alpha\n".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("sleep 2; echo alpha".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };
    let output_b = ExecCommandToolOutput {
        event_call_id: "exec-call-b".to_string(),
        chunk_id: "chunk-b".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"beta\n".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("sleep 1; echo beta".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };
    let invocation_b = invocation_for_payload("write_stdin", "write-call-b", payload.clone()).await;
    let invocation_a = invocation_for_payload("write_stdin", "write-call-a", payload).await;
    let handler = WriteStdinHandler;

    let payloads = [
        handler.post_tool_use_payload(&invocation_b, &output_b),
        handler.post_tool_use_payload(&invocation_a, &output_a),
    ];

    assert_eq!(
        payloads,
        [
            Some(crate::tools::registry::PostToolUsePayload {
                tool_name: HookToolName::exec_command(),
                tool_use_id: "exec-call-b".to_string(),
                tool_input: serde_json::json!({ "command": "sleep 1; echo beta" }),
                tool_response: serde_json::json!("beta\n"),
            }),
            Some(crate::tools::registry::PostToolUsePayload {
                tool_name: HookToolName::exec_command(),
                tool_use_id: "exec-call-a".to_string(),
                tool_input: serde_json::json!({ "command": "sleep 2; echo alpha" }),
                tool_response: serde_json::json!("alpha\n"),
            }),
        ]
    );
}
