use super::exec_command::attach_powershell_failure_advisory;
use super::exec_command::validate_and_consume_remote_shell;
use super::*;
use crate::shell::ShellType;
use crate::shell::default_user_shell;
use codex_exec_server::Environment;
use codex_protocol::models::PermissionProfile;
use codex_tools::ToolExecutor;
use codex_tools::UnifiedExecShellMode;
use codex_tools::ZshForkConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;

use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
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
    let repair_notice = output
        .repair_notice
        .as_deref()
        .expect("PowerShell failure should expose model recovery guidance");
    assert!(repair_notice.starts_with(existing_repair_notice));
    assert!(repair_notice.contains("retry with `kind: \"powershell_script\"`"));

    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
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
        turn,
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
        turn,
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
        turn,
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
        &UnifiedExecShellMode::Direct,
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
        &UnifiedExecShellMode::Direct,
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
        &UnifiedExecShellMode::Direct,
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
        &UnifiedExecShellMode::Direct,
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
        &UnifiedExecShellMode::Direct,
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
                "args": ["--ignorecase", "--version"]
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
    assert!(code_mode["raw_output_artifact"].is_string());

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
async fn intercepted_apply_patch_failure_releases_process_id_and_counts_retry_failure() {
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
                turn: Arc::clone(&turn),
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
    let blocked = match handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn: Arc::clone(&turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "intercept-failure-blocked".to_string(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
            source: ToolCallSource::Direct,
            payload: payload_with_output_only_change,
        })
        .await
    {
        Ok(_) => panic!("third identical failure must be blocked"),
        Err(err) => err,
    };
    assert!(blocked.to_string().contains("Command blocked"));

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
    assert_eq!(post_hook.tool_name, HookToolName::bash());
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
async fn unpolled_background_failure_finalizes_artifact_and_attempt_ledger() {
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Python is required by the KD4 test environment");
    let script =
        "import time; time.sleep(2.5); print('BACKGROUND_FINAL_MARKER'); raise SystemExit(7)";
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
    let invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
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
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .expect("numeric background process id");
    let attempt_key = session
        .services
        .command_execution
        .running_process(process_id)
        .await
        .expect("background process must be tracked while it is running")
        .key;
    let artifact_path = PathBuf::from(
        code_mode["raw_output_artifact"]
            .as_str()
            .expect("raw output artifact path"),
    );

    let mut retained = String::new();
    let mut consecutive_failures = 0;
    for _ in 0..100 {
        retained = tokio::fs::read_to_string(&artifact_path)
            .await
            .unwrap_or_default();
        consecutive_failures = session
            .services
            .command_execution
            .consecutive_failures(&attempt_key)
            .await;
        if retained.contains("BACKGROUND_FINAL_MARKER") && consecutive_failures == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(retained.contains("BACKGROUND_FINAL_MARKER"));
    assert_eq!(consecutive_failures, 1);
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
    let invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
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
    let artifact_path = PathBuf::from(
        code_mode["raw_output_artifact"]
            .as_str()
            .expect("raw output artifact path"),
    );
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
fn test_get_command_respects_explicit_bash_shell() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "/bin/bash"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert_eq!(args.shell.as_deref(), Some("/bin/bash"));

    let resolved = get_command(
        &args,
        Arc::new(default_user_shell()),
        &UnifiedExecShellMode::Direct,
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
    let powershell_path = temp_dir.path().join(if cfg!(windows) {
        "powershell.exe"
    } else {
        "powershell"
    });
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
        &UnifiedExecShellMode::Direct,
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
        &UnifiedExecShellMode::Direct,
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
        &UnifiedExecShellMode::Direct,
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

#[test]
fn test_get_command_rejects_explicit_shell_in_zsh_fork_mode() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "/bin/bash"}"#;
    let args: ExecCommandArgs = parse_arguments(json)?;
    let shell_zsh_path = AbsolutePathBuf::from_absolute_path(if cfg!(windows) {
        r"C:\opt\codex\zsh"
    } else {
        "/opt/codex/zsh"
    })?;
    let shell_mode = UnifiedExecShellMode::ZshFork(ZshForkConfig {
        shell_zsh_path,
        main_execve_wrapper_exe: AbsolutePathBuf::from_absolute_path(if cfg!(windows) {
            r"C:\opt\codex\codex-execve-wrapper"
        } else {
            "/opt/codex/codex-execve-wrapper"
        })?,
    });

    let err = get_command(
        &args,
        Arc::new(default_user_shell()),
        &shell_mode,
        /*allow_login_shell*/ true,
        /*environment_is_remote*/ false,
    )
    .expect_err("explicit shell should be rejected");

    assert!(
        err.contains("`shell` is not supported for local zsh-fork exec"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn shell_mode_for_environment_uses_direct_mode_for_remote_environments() -> anyhow::Result<()>
{
    let shell_zsh_path = AbsolutePathBuf::from_absolute_path(if cfg!(windows) {
        r"C:\opt\codex\zsh"
    } else {
        "/opt/codex/zsh"
    })?;
    let shell_mode = UnifiedExecShellMode::ZshFork(ZshForkConfig {
        shell_zsh_path,
        main_execve_wrapper_exe: AbsolutePathBuf::from_absolute_path(if cfg!(windows) {
            r"C:\opt\codex\codex-execve-wrapper"
        } else {
            "/opt/codex/codex-execve-wrapper"
        })?,
    });
    let local_environment = Environment::default_for_tests();
    let remote_environment =
        Environment::create_for_tests(Some("ws://127.0.0.1:1/remote-exec-server".to_string()))?;

    assert_eq!(
        shell_mode_for_environment(&shell_mode, &local_environment),
        shell_mode
    );
    assert_eq!(
        shell_mode_for_environment(&shell_mode, &remote_environment),
        UnifiedExecShellMode::Direct
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
        turn,
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
            tool_name: HookToolName::bash(),
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
async fn exec_command_hook_noop_preserves_direct_argv_and_rejects_text_rewrite() {
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
        .expect("argv invocation should expose hook input")
        .tool_input;

    let rewritten = handler
        .with_updated_hook_input(invocation.clone(), updated_input)
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
        &UnifiedExecShellMode::Direct,
        /*allow_login_shell*/ false,
        /*environment_is_remote*/ false,
    )
    .expect("preserved argv should resolve directly");
    assert_eq!(resolved.command, vec!["rg", "--files"]);
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
            turn,
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
            tool_name: HookToolName::bash(),
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
            tool_name: HookToolName::bash(),
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
            tool_name: HookToolName::bash(),
            tool_use_id: "exec-call-45".to_string(),
            tool_input: serde_json::json!({ "command": "sleep 1; echo finished" }),
            tool_response: serde_json::json!("finished\n"),
        })
    );
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
                tool_name: HookToolName::bash(),
                tool_use_id: "exec-call-b".to_string(),
                tool_input: serde_json::json!({ "command": "sleep 1; echo beta" }),
                tool_response: serde_json::json!("beta\n"),
            }),
            Some(crate::tools::registry::PostToolUsePayload {
                tool_name: HookToolName::bash(),
                tool_use_id: "exec-call-a".to_string(),
                tool_input: serde_json::json!({ "command": "sleep 2; echo alpha" }),
                tool_response: serde_json::json!("alpha\n"),
            }),
        ]
    );
}
