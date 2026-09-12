use std::path::PathBuf;
use std::sync::Arc;

use codex_exec_server::Environment;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::ShellCommandToolCallParams;
use pretty_assertions::assert_eq;

use crate::config::PermissionProfileSnapshot;
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::sandboxing::SandboxPermissions;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::ShellCommandHandler;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::shell::shell_command::resolve_command_shell;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::validation_admission::prohibited_skip_for;
use codex_shell_command::is_safe_command::is_known_safe_command;
use codex_shell_command::powershell::try_find_powershell_executable_blocking;
use codex_shell_command::powershell::try_find_pwsh_executable_blocking;
use codex_tools::ToolExecutor;
use codex_utils_path_uri::PathUri;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::sync::Mutex;

use super::parse_shell_command_hook_invocation;
use super::shell_command::effective_stall_timeout_ms;
use super::shell_failure_sampling_signal;
use super::shell_sampling_signal;

#[tokio::test]
async fn late_validation_denial_finishes_the_started_shell_event() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let invocation = CommandInvocation::Argv {
        program: "cargo".to_string(),
        args: vec!["test".to_string()],
    };
    let skipped = {
        let mut authorization = turn.validation_authorization.write().await;
        *authorization = crate::validation_admission::ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests"));
        prohibited_skip_for(&authorization, &invocation, true)
            .expect("test denial suppresses the validation")
    };
    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    let call_id = "late-validation-denial";
    let command = vec!["cargo".to_string(), "test".to_string()];
    let emitter = ToolEmitter::shell(
        command,
        turn.cwd().clone(),
        codex_protocol::protocol::ExecCommandSource::Agent,
        turn.environments
            .primary()
            .expect("primary environment")
            .environment_id
            .clone(),
    );
    emitter
        .begin(ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            call_id,
            Some(&tracker),
        ))
        .await;

    let result = super::finish_validation_skip_after_begin(
        &emitter,
        &session,
        &turn,
        call_id,
        &crate::tools::context::ToolCallSource::Direct,
        Some(&tracker),
        skipped,
    )
    .await
    .expect("denial produces a normal skipped tool result");
    assert_eq!(
        result.validation_execution_outcome,
        super::ValidationExecutionOutcome::NotExecuted
    );

    let mut saw_begin = false;
    let mut saw_end = false;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !saw_end {
            let event = rx_event.recv().await.expect("event channel remains open");
            match event.msg {
                codex_protocol::protocol::EventMsg::ExecCommandBegin(event)
                    if event.call_id == call_id =>
                {
                    saw_begin = true;
                }
                codex_protocol::protocol::EventMsg::ExecCommandEnd(event)
                    if event.call_id == call_id =>
                {
                    saw_end = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("a denied validation closes its started shell event");
    assert!(saw_begin);
    assert!(saw_end);
    assert_eq!(
        turn.turn_timing_state
            .complete_snapshot()
            .protocol_timing()
            .counters
            .suppressed_validation_output_count,
        1,
        "a post-admission denial must retain suppressed-by-user timing",
    );
}

#[test]
fn orchestration_correctness_stall_timeout_is_opt_in() {
    assert_eq!(effective_stall_timeout_ms(None, None), None);
    assert_eq!(effective_stall_timeout_ms(Some(60_001), None), None);
    assert_eq!(
        effective_stall_timeout_ms(Some(300_000), Some(25_000)),
        Some(25_000)
    );
    assert_eq!(effective_stall_timeout_ms(Some(300_000), Some(0)), None);
    assert_eq!(effective_stall_timeout_ms(Some(30_000), Some(30_000)), None);
}

#[test]
fn workspace_operation_root_reuses_the_preclassified_inspection_result() {
    let root = std::path::PathBuf::from("repo");

    assert_eq!(
        super::workspace_operation_root_if_needed(false, true, root.clone()),
        None
    );
    assert_eq!(
        super::workspace_operation_root_if_needed(false, false, root.clone()),
        Some(root.clone())
    );
    assert_eq!(
        super::workspace_operation_root_if_needed(true, true, root.clone()),
        Some(root)
    );
}

#[tokio::test]
async fn shell_pipeline_validation_is_denied_before_execution() {
    let (session, turn) = make_session_and_context().await;
    {
        let mut authorization = turn.validation_authorization.write().await;
        *authorization = crate::validation_admission::ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests"));
    }
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: json!({
            "kind": "script",
            "command": "cargo test | cargo --version"
        })
        .to_string(),
    };

    let output = ShellCommandHandler::default()
        .handle(ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "shell-pipeline-validation-denied".to_string(),
            tool_name: codex_tools::ToolName::plain("shell_command"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("the denied pipeline should return a structured skip");
    let structured = output
        .post_tool_use_response("shell-pipeline-validation-denied", &payload)
        .expect("the validation skip should retain its structured result");

    assert_eq!(structured["reason"], "user_prohibited_validation");
    assert_eq!(structured["operation"], "test");
    assert_eq!(structured["command_was_executed"], false);
}

#[test]
fn shell_failure_sampling_signal_is_stable_and_distinguishes_outcomes() {
    let key = CommandAttemptKey::new(
        "shell_command",
        "local",
        "C:/repo",
        &["git".to_string(), "status".to_string()],
    );
    let first = shell_failure_sampling_signal(Some(&key), "git status", Some(1))
        .expect("nonzero exit should carry failure evidence");
    let repeated = shell_failure_sampling_signal(Some(&key), "git status", Some(1))
        .expect("repeated nonzero exit should carry failure evidence");
    let timeout = shell_failure_sampling_signal(Some(&key), "git status", None)
        .expect("timeout should carry failure evidence");

    assert_eq!(first, repeated);
    assert_ne!(first, timeout);
    assert!(
        first
            .pointer("/failure/fingerprint")
            .and_then(JsonValue::as_str)
            .is_some_and(|fingerprint| fingerprint.starts_with("shell."))
    );
    assert!(shell_failure_sampling_signal(Some(&key), "git status", Some(0)).is_none());
}

#[test]
fn successful_shell_sampling_signal_uses_canonical_output() {
    let key = CommandAttemptKey::new(
        "shell_command",
        "local",
        "C:/repo",
        &["git".to_string(), "status".to_string()],
    );
    let first = shell_sampling_signal(Some(&key), "git status", Some(0), Some(b"clean\n"));
    let repeated = shell_sampling_signal(Some(&key), "git status", Some(0), Some(b"clean\n"));

    assert!(first.is_some());
    assert_eq!(first, repeated);
}

#[test]
fn validation_diagnostic_ranges_are_exact_and_bounded() {
    let range = super::validation_diagnostic_range("validation:diagnostics", b"first\nsecond\n")
        .expect("bounded diagnostics");
    assert_eq!(range.id, "validation:diagnostics");
    assert_eq!((range.start_line, range.end_line), (1, 2));

    assert!(super::validation_diagnostic_range("", b"failure\n").is_none());
    assert!(
        super::validation_diagnostic_range("validation:diagnostics", &vec![b'x'; 12 * 1024 + 1],)
            .is_none()
    );
    let too_many_lines = std::iter::repeat_n("line", 201)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        super::validation_diagnostic_range("validation:diagnostics", too_many_lines.as_bytes(),)
            .is_none()
    );
}

#[test]
fn direct_normalization_requires_the_same_authorization_envelope() {
    use crate::tools::sandboxing::ExecApprovalRequirement;

    let sandboxed_skip = ExecApprovalRequirement::Skip {
        bypass_sandbox: false,
        proposed_execpolicy_amendment: None,
    };
    let unsandboxed_skip = ExecApprovalRequirement::Skip {
        bypass_sandbox: true,
        proposed_execpolicy_amendment: None,
    };
    let needs_approval = ExecApprovalRequirement::NeedsApproval {
        reason: Some("approval required".to_string()),
        proposed_execpolicy_amendment: None,
    };
    let differently_explained_approval = ExecApprovalRequirement::NeedsApproval {
        reason: Some("canonical target requires approval".to_string()),
        proposed_execpolicy_amendment: None,
    };
    let forbidden = ExecApprovalRequirement::Forbidden {
        reason: "executable-specific denial".to_string(),
    };

    assert!(super::same_exec_authorization_envelope(
        &sandboxed_skip,
        &sandboxed_skip,
    ));
    assert!(!super::same_exec_authorization_envelope(
        &needs_approval,
        &differently_explained_approval,
    ));
    assert!(!super::same_exec_authorization_envelope(
        &sandboxed_skip,
        &unsandboxed_skip,
    ));
    assert!(!super::same_exec_authorization_envelope(
        &sandboxed_skip,
        &needs_approval,
    ));
    assert!(!super::same_exec_authorization_envelope(
        &needs_approval,
        &forbidden,
    ));
}

#[test]
fn shell_metadata_does_not_change_the_output_payload() {
    let mut content = "Exit code: 0\nWall time: 0.1 seconds\nOutput:\nline1\n".to_string();

    super::insert_metadata_before_output(&mut content, "Raw output artifact: raw.log");

    assert_eq!(
        content,
        "Exit code: 0\nWall time: 0.1 seconds\nRaw output artifact: raw.log\nOutput:\nline1\n"
    );
    assert_eq!(
        content.split_once("Output:\n").map(|(_, output)| output),
        Some("line1\n")
    );
}

#[test]
fn retry_guard_counts_operational_rejections_but_not_user_declines() {
    let operational: Result<
        codex_protocol::exec_output::ExecToolCallOutput,
        crate::tools::sandboxing::ToolError,
    > = Err(crate::tools::sandboxing::ToolError::Rejected(
        "sandbox setup failed".to_string(),
    ));
    let user_declined: Result<
        codex_protocol::exec_output::ExecToolCallOutput,
        crate::tools::sandboxing::ToolError,
    > = Err(crate::tools::sandboxing::ToolError::Denied(
        "rejected by user".to_string(),
    ));

    assert_eq!(super::retry_exit_code(&operational), Some(-1));
    assert_eq!(super::retry_exit_code(&user_declined), None);
}

#[test]
fn output_bearing_shell_failures_continue_through_structured_finalization() {
    let projected = "Exit code: 7\nOutput:\nfailed".to_string();

    assert_eq!(
        super::recover_output_bearing_shell_content(
            Err(crate::FunctionCallError::RespondToModel(projected.clone())),
            true,
        ),
        Ok(projected)
    );
    assert_eq!(
        super::recover_output_bearing_shell_content(
            Err(crate::FunctionCallError::RespondToModel(
                "pre-execution rejection".to_string(),
            )),
            false,
        ),
        Err(crate::FunctionCallError::RespondToModel(
            "pre-execution rejection".to_string(),
        ))
    );
    assert_eq!(
        super::recover_output_bearing_shell_content(
            Err(crate::FunctionCallError::DeniedToModel(
                "declined by user".to_string(),
            )),
            true,
        ),
        Err(crate::FunctionCallError::DeniedToModel(
            "declined by user".to_string(),
        ))
    );
}

#[test]
fn nonzero_shell_output_has_failure_outcome() {
    let output = codex_protocol::exec_output::ExecToolCallOutput {
        exit_code: 7,
        ..Default::default()
    };

    assert!(super::shell_result_has_execution_output(
        &Ok(output.clone())
    ));
    assert_eq!(
        super::shell_tool_outcome(&Ok(output)),
        codex_tools::ToolOutputOutcome::Failure
    );
}

#[test]
fn retained_validation_attempt_preserves_output_and_late_skip_timing() {
    let retained_attempt = codex_protocol::exec_output::ExecToolCallOutput {
        exit_code: 126,
        ..Default::default()
    };
    let user_declined: Result<
        codex_protocol::exec_output::ExecToolCallOutput,
        crate::tools::sandboxing::ToolError,
    > = Err(crate::tools::sandboxing::ToolError::Denied(
        "rejected by user".to_string(),
    ));
    assert_eq!(
        super::shell_validation_execution_output(&user_declined, Some(&retained_attempt))
            .map(|output| output.exit_code),
        Some(126),
        "a retry refusal must not hide the sandboxed validation attempt",
    );

    let invocation = CommandInvocation::Argv {
        program: "cargo".to_string(),
        args: vec!["test".to_string()],
    };
    let mut authorization = crate::validation_admission::ValidationAuthorization::enabled();
    assert!(authorization.update_from_user_input("do not run tests"));
    let skipped = prohibited_skip_for(&authorization, &invocation, true)
        .expect("test denial suppresses the validation");
    let late_skip = Err(crate::tools::sandboxing::ToolError::ValidationSkipped(
        skipped,
    ));
    let timing = crate::turn_timing::TurnTimingState::default();
    super::record_retained_validation_skip(&timing, &late_skip, Some(&retained_attempt));

    assert_eq!(
        timing
            .complete_snapshot()
            .protocol_timing()
            .counters
            .suppressed_validation_output_count,
        1,
    );
    assert!(super::unexecuted_validation_skip(&late_skip, false).is_some());
    assert!(super::unexecuted_validation_skip(&late_skip, true).is_none());
    assert_eq!(
        super::restore_retained_validation_attempt(late_skip, Some(&retained_attempt))
            .expect("retained attempt becomes the terminal output")
            .exit_code,
        126,
    );
}

#[test]
fn legacy_shell_projection_metadata_keeps_exact_bytes_status_and_context() {
    let raw = vec![b'o', b'k', b'\n', 0xff];
    let output = super::LegacyShellToolOutput {
        validation: None,
        inner: FunctionToolOutput::from_text("bounded shell output".to_string(), Some(false)),
        canonical_output: Some(raw.clone()),
        exit_code: Some(7),
        call_id: "shell-call".to_string(),
        validation_failure: false,
    };
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };

    assert_eq!(
        output
            .canonical_result(&payload)
            .expect("canonical shell bytes")
            .bytes,
        raw
    );
    let projection = output
        .projection_metadata()
        .expect("typed shell projection");
    assert_eq!(projection.spillable_text.len(), 1);
    assert_eq!(projection.essential_inline["exit_code"], 7);
    assert_eq!(projection.essential_inline["call_id"], "shell-call");
    assert!(projection.fragments.iter().any(|fragment| {
        fragment.kind == codex_tools::ToolOutputProjectionFragmentKind::ProcessFinalStatus
    }));
    assert!(projection.fragments.iter().any(|fragment| {
        fragment.kind == codex_tools::ToolOutputProjectionFragmentKind::ContextualSpillableText
            && fragment.text == "bounded shell output"
    }));
}

#[test]
fn independent_review_rejects_non_inspection_before_validation() {
    let reviewer = codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::Review,
    );
    assert!(
        crate::agent::task_capabilities::validate_independent_review_shell(
            &reviewer, false, false, false
        )
        .is_err()
    );
}

#[test]
fn independent_review_disables_login_shells() {
    let reviewer = codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            parent_thread_id: codex_protocol::ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("reviewer".to_string()),
        },
    );

    assert!(!ShellCommandHandler::effective_allow_login_shell(
        &reviewer, true
    ));
    assert!(ShellCommandHandler::effective_allow_login_shell(
        &codex_protocol::protocol::SessionSource::Cli,
        true
    ));
}

#[test]
fn independent_review_powershell_safety_args_disable_profiles() {
    let Some(powershell) = try_find_powershell_executable_blocking() else {
        return;
    };
    let reviewer = codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            parent_thread_id: codex_protocol::ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("reviewer".to_string()),
        },
    );
    let use_login_shell = ShellCommandHandler::resolve_use_login_shell(
        None,
        ShellCommandHandler::effective_allow_login_shell(&reviewer, true),
    )
    .expect("independent reviewers should use a non-login shell");
    let shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: powershell.to_path_buf(),
    };
    let safety_command =
        CommandInvocation::PowerShellScript("Get-Content -LiteralPath Cargo.toml".to_string())
            .to_safety_args(&shell, use_login_shell)
            .expect("PowerShell safety args");

    assert!(safety_command.iter().any(|arg| arg == "-NoProfile"));
    assert!(is_known_safe_command(&safety_command));
}

/// The logic for is_known_safe_command() has heuristics for known shells,
/// so we must ensure the commands generated by [ShellCommandHandler] can be
/// recognized as safe if the `command` is safe.
#[test]
fn commands_generated_by_shell_command_handler_can_be_matched_by_is_known_safe_command() {
    if let Some(path) = try_find_powershell_executable_blocking() {
        let powershell = Shell {
            shell_type: ShellType::PowerShell,
            shell_path: path.to_path_buf(),
        };
        assert_safe(&powershell, "ls -Name");
    }

    if let Some(path) = try_find_pwsh_executable_blocking() {
        let pwsh = Shell {
            shell_type: ShellType::PowerShell,
            shell_path: path.to_path_buf(),
        };
        assert_safe(&pwsh, "ls -Name");
    }
}

fn assert_safe(shell: &Shell, command: &str) {
    let login_command = shell
        .derive_exec_args(command, /*use_login_shell*/ true)
        .expect("Windows shell args");
    let login_command_is_safe = is_known_safe_command(&login_command);
    if shell.shell_type == ShellType::PowerShell {
        assert!(!login_command_is_safe);
    } else {
        assert!(login_command_is_safe);
    }
    let non_login_command = shell
        .derive_exec_args(command, /*use_login_shell*/ false)
        .expect("Windows shell args");
    assert!(is_known_safe_command(&non_login_command));
}

#[test]
fn powershell_script_rejects_non_powershell_remote_environment() {
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::try_from(
        std::env::current_dir().expect("current dir"),
    )
    .expect("absolute current dir");
    let remote = Arc::new(
        Environment::create_for_tests(Some("ws://127.0.0.1:1/remote".to_string()))
            .expect("remote environment"),
    );
    let environment = TurnEnvironment::new(
        "remote".to_string(),
        remote,
        PathUri::from_abs_path(&cwd),
        Some(Shell {
            shell_type: ShellType::Cmd,
            shell_path: PathBuf::from("cmd.exe"),
        }),
    );
    let invocation = CommandInvocation::PowerShellScript("Get-ChildItem".to_string());
    let session_shell = Shell {
        shell_type: ShellType::Cmd,
        shell_path: PathBuf::from("cmd.exe"),
    };

    let err = resolve_command_shell(&invocation, &environment, &session_shell)
        .expect_err("host PowerShell must not be substituted for a remote shell");
    assert!(
        err.to_string()
            .contains("remote environment to report PowerShell")
    );
}

#[tokio::test]
async fn shell_command_handler_to_exec_params_uses_selected_environment() {
    let (session, mut turn_context) = make_session_and_context().await;
    let permission_profile = turn_context.config.permissions.permission_profile().clone();
    Arc::make_mut(&mut turn_context.config)
        .permissions
        .set_permission_profile_from_session_snapshot(PermissionProfileSnapshot::active(
            permission_profile,
            ActivePermissionProfile::new("test-profile"),
        ))
        .expect("set active permission profile");

    let command = "echo hello".to_string();
    let workdir = Some("subdir".to_string());
    let login = None;
    let timeout_ms = Some(1234);
    let sandbox_permissions = SandboxPermissions::RequireEscalated;
    let justification = Some("because tests".to_string());

    let selected_shell = Shell {
        shell_type: ShellType::Cmd,
        shell_path: PathBuf::from("cmd.exe"),
    };
    let expected_command = selected_shell
        .derive_exec_args(&command, /*use_login_shell*/ true)
        .expect("Cmd args");
    let selected_cwd = turn_context.config.cwd.join("selected-environment");
    let expected_cwd = selected_cwd.join("subdir");
    let selected_environment = TurnEnvironment::new(
        "selected-environment".to_string(),
        Arc::clone(
            &turn_context
                .environments
                .primary()
                .expect("primary environment")
                .environment,
        ),
        PathUri::from_abs_path(&selected_cwd),
        Some(selected_shell),
    );
    let mut expected_env = create_env(
        &turn_context.config.permissions.shell_environment_policy,
        Some(session.thread_id),
    );
    let active_permission_profile = turn_context.config.permissions.active_permission_profile();
    inject_permission_profile_env(&mut expected_env, active_permission_profile.as_ref());

    let params = ShellCommandToolCallParams {
        command: Some(command.clone()),
        kind: None,
        program: None,
        args: None,
        script_body: None,
        workdir,
        login,
        timeout_ms,
        stall_timeout_ms: None,
        sandbox_permissions: Some(sandbox_permissions),
        additional_permissions: None,
        prefix_rule: None,
        justification: justification.clone(),
        validation: None,
        force_fresh: None,
    };

    let exec_params = ShellCommandHandler::to_exec_params(
        &params,
        &CommandInvocation::Script(command),
        &session,
        &turn_context,
        &selected_environment,
        expected_cwd.clone(),
        /*allow_login_shell*/ true,
    )
    .expect("login shells should be allowed");

    // ExecParams cannot derive Eq due to the CancellationToken field, so we manually compare the fields.
    assert_eq!(exec_params.command, expected_command);
    assert_eq!(exec_params.cwd, expected_cwd);
    assert_eq!(exec_params.env, expected_env);
    assert_eq!(
        exec_params.env.get(CODEX_PERMISSION_PROFILE_ENV_VAR),
        active_permission_profile
            .as_ref()
            .map(|profile| &profile.id)
    );
    assert_eq!(exec_params.network, turn_context.network);
    assert_eq!(
        exec_params.network_environment_id.as_deref(),
        Some("selected-environment")
    );
    assert_eq!(exec_params.expiration.timeout_ms(), timeout_ms);
    assert_eq!(exec_params.sandbox_permissions, sandbox_permissions);
    assert_eq!(exec_params.justification, justification);
    assert_eq!(exec_params.arg0, None);
}

#[test]
fn shell_command_handler_respects_explicit_login_flag() {
    let shell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("powershell.exe"),
    };

    let login_command = ShellCommandHandler::base_command(
        &shell,
        "echo login shell",
        /*use_login_shell*/ true,
    );
    assert_eq!(
        login_command,
        shell
            .derive_exec_args("echo login shell", /*use_login_shell*/ true)
            .expect("PowerShell args")
    );

    let non_login_command = ShellCommandHandler::base_command(
        &shell,
        "echo non login shell",
        /*use_login_shell*/ false,
    );
    assert_eq!(
        non_login_command,
        shell
            .derive_exec_args("echo non login shell", /*use_login_shell*/ false)
            .expect("PowerShell args")
    );
}

#[tokio::test]
async fn shell_command_handler_defaults_to_non_login_when_disallowed() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_environment = turn_context
        .environments
        .primary()
        .expect("primary environment");
    let cwd = turn_environment
        .cwd()
        .to_abs_path()
        .expect("native environment cwd");
    let params = ShellCommandToolCallParams {
        command: Some("echo hello".to_string()),
        kind: None,
        program: None,
        args: None,
        script_body: None,
        workdir: None,
        login: None,
        timeout_ms: None,
        stall_timeout_ms: None,
        sandbox_permissions: None,
        additional_permissions: None,
        prefix_rule: None,
        justification: None,
        validation: None,
        force_fresh: None,
    };

    let exec_params = ShellCommandHandler::to_exec_params(
        &params,
        &CommandInvocation::Script("echo hello".to_string()),
        &session,
        &turn_context,
        turn_environment,
        cwd,
        /*allow_login_shell*/ false,
    )
    .expect("non-login shells should still be allowed");

    assert_eq!(
        exec_params.command,
        session
            .user_shell()
            .derive_exec_args("echo hello", /*use_login_shell*/ false)
            .expect("default Windows shell args")
    );
}

#[tokio::test]
async fn shell_command_exec_params_reuse_the_resolved_shell() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_environment = turn_context
        .environments
        .primary()
        .expect("primary environment");
    let cwd = turn_environment
        .cwd()
        .to_abs_path()
        .expect("native environment cwd");
    let params = ShellCommandToolCallParams {
        command: Some("echo hello".to_string()),
        kind: None,
        program: None,
        args: None,
        script_body: None,
        workdir: None,
        login: Some(false),
        timeout_ms: None,
        stall_timeout_ms: None,
        sandbox_permissions: None,
        additional_permissions: None,
        prefix_rule: None,
        justification: None,
        validation: None,
        force_fresh: None,
    };
    let resolved_shell = Shell {
        shell_type: ShellType::Cmd,
        shell_path: PathBuf::from("cmd.exe"),
    };

    let exec_params = ShellCommandHandler::to_exec_params_with_shell(
        &params,
        &CommandInvocation::Script("echo hello".to_string()),
        &session,
        &turn_context,
        turn_environment,
        cwd,
        /*allow_login_shell*/ false,
        &resolved_shell,
    )
    .expect("resolved shell should be accepted without another lookup");

    assert_eq!(
        exec_params.command,
        resolved_shell
            .derive_exec_args("echo hello", /*use_login_shell*/ false)
            .expect("Cmd args")
    );
}

#[tokio::test]
async fn shell_command_handler_preserves_structured_argv_shape() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_environment = turn_context
        .environments
        .primary()
        .expect("primary environment");
    let cwd = turn_environment
        .cwd()
        .to_abs_path()
        .expect("native environment cwd");
    let params = ShellCommandToolCallParams {
        command: None,
        kind: Some("argv".to_string()),
        program: Some("python".to_string()),
        args: Some(vec![
            "script with spaces.py".to_string(),
            "quote\"inside".to_string(),
            String::new(),
            "Grüße 世界".to_string(),
        ]),
        script_body: None,
        workdir: None,
        login: None,
        timeout_ms: None,
        stall_timeout_ms: None,
        sandbox_permissions: None,
        additional_permissions: None,
        prefix_rule: None,
        justification: None,
        validation: None,
        force_fresh: None,
    };
    let invocation = CommandInvocation::from_parts(
        "shell_command",
        "command",
        params.command.as_deref(),
        params.kind.as_deref(),
        params.program.as_deref(),
        params.args.as_deref(),
        params.script_body.as_deref(),
    )
    .expect("valid argv shape");

    let exec_params = ShellCommandHandler::to_exec_params(
        &params,
        &invocation,
        &session,
        &turn_context,
        turn_environment,
        cwd,
        /*allow_login_shell*/ false,
    )
    .expect("argv command should resolve");

    assert_eq!(
        exec_params.command,
        vec![
            "python",
            "script with spaces.py",
            "quote\"inside",
            "",
            "Grüße 世界",
        ]
    );
}

#[test]
fn shell_command_handler_rejects_login_when_disallowed() {
    let err =
        ShellCommandHandler::resolve_use_login_shell(Some(true), /*allow_login_shell*/ false)
            .expect_err("explicit login should be rejected");

    assert!(
        err.to_string()
            .contains("login shell is disabled by config"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn shell_command_pre_tool_use_payload_uses_raw_command() {
    let payload = ToolPayload::Function {
        arguments: json!({ "command": "printf shell command" }).to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let handler = ShellCommandHandler::default();

    assert_eq!(
        handler.pre_tool_use_payload(&ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-42".to_string(),
            tool_name: codex_tools::ToolName::plain("shell_command"),
            source: crate::tools::context::ToolCallSource::Direct,
            payload,
        }),
        Some(crate::tools::registry::PreToolUsePayload {
            tool_name: HookToolName::shell_command(),
            tool_input: json!({ "command": "printf shell command" }),
        })
    );
}

#[tokio::test]
async fn shell_command_hook_rewrite_preserves_powershell_script_mode() {
    let payload = ToolPayload::Function {
        arguments: json!({
            "kind": "powershell_script",
            "script_body": "Write-Output before",
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
    let handler = ShellCommandHandler::default();
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "powershell-hook-rewrite".to_string(),
        tool_name: codex_tools::ToolName::plain("shell_command"),
        source: ToolCallSource::Direct,
        payload,
    };

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(crate::tools::registry::PreToolUsePayload {
            tool_name: HookToolName::shell_command(),
            tool_input: json!({
                "command": "Write-Output before",
                "kind": "powershell_script",
                "script_body": "Write-Output before",
            }),
        })
    );

    let rewritten = handler
        .with_updated_hook_input(
            invocation,
            json!({
                "kind": "powershell_script",
                "script_body": "Write-Output after",
            }),
        )
        .expect("PowerShell hook rewrite should preserve structured mode");
    let ToolPayload::Function { arguments } = rewritten.payload else {
        panic!("rewritten shell_command payload should remain function-shaped");
    };
    let rewritten_arguments: serde_json::Value =
        serde_json::from_str(&arguments).expect("rewritten arguments should remain valid JSON");
    let command = parse_shell_command_hook_invocation(&arguments)
        .expect("rewritten PowerShell command should remain structured");
    let powershell = Shell {
        shell_type: ShellType::PowerShell,
        shell_path: PathBuf::from("pwsh"),
    };
    let exec_args = command
        .to_exec_args(&powershell, /*use_login_shell*/ false)
        .expect("PowerShell args");

    assert_eq!(rewritten_arguments["kind"], "powershell_script");
    assert_eq!(rewritten_arguments["script_body"], "Write-Output after");
    assert_eq!(
        rewritten_arguments["additional_permissions"],
        json!({
            "file_system": {
                "write": ["relative-output"]
            }
        })
    );
    assert!(exec_args.iter().any(|arg| arg == "-EncodedCommand"));
    assert!(!exec_args.iter().any(|arg| arg == "Write-Output after"));
}

#[tokio::test]
async fn shell_command_hook_rewrite_preserves_direct_argv_mode() {
    let payload = ToolPayload::Function {
        arguments: json!({
            "kind": "argv",
            "program": "rg",
            "args": ["--files"],
            "timeout_ms": 1234
        })
        .to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let handler = ShellCommandHandler::default();
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "argv-hook-rewrite".to_string(),
        tool_name: codex_tools::ToolName::plain("shell_command"),
        source: ToolCallSource::Direct,
        payload,
    };

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(crate::tools::registry::PreToolUsePayload {
            tool_name: HookToolName::shell_command(),
            tool_input: json!({
                "command": "rg --files",
                "kind": "argv",
                "program": "rg",
                "args": ["--files"],
            }),
        })
    );

    let rewritten = handler
        .with_updated_hook_input(
            invocation,
            json!({
                "kind": "argv",
                "program": "kds",
                "args": ["--agent", "--", "rg", "--files"],
            }),
        )
        .expect("structured argv hook rewrite should retain argv mode");
    let ToolPayload::Function { arguments } = rewritten.payload else {
        panic!("rewritten shell_command payload should remain function-shaped");
    };
    let rewritten_arguments: serde_json::Value =
        serde_json::from_str(&arguments).expect("rewritten arguments should remain valid JSON");
    let command = parse_shell_command_hook_invocation(&arguments)
        .expect("rewritten direct argv command should remain structured");

    assert_eq!(rewritten_arguments["kind"], "argv");
    assert_eq!(rewritten_arguments["program"], "kds");
    assert_eq!(
        rewritten_arguments["args"],
        json!(["--agent", "--", "rg", "--files"])
    );
    assert_eq!(rewritten_arguments["timeout_ms"], 1234);
    assert_eq!(
        command.to_direct_argv(),
        Some(vec![
            "kds".to_string(),
            "--agent".to_string(),
            "--".to_string(),
            "rg".to_string(),
            "--files".to_string(),
        ])
    );
}

#[tokio::test]
async fn shell_command_active_path_applies_read_only_repair_and_retains_output() {
    let payload = ToolPayload::Function {
        arguments: json!({
            "kind": "argv",
            "program": "rg",
            "args": ["--ignorecase", "--version"]
        })
        .to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let handler = ShellCommandHandler::default();
    let output = handler
        .handle(ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "shell-preflight-repair".to_string(),
            tool_name: codex_tools::ToolName::plain("shell_command"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("read-only typo should be repaired and executed");

    let rendered = output.code_mode_result(&payload).to_string();
    assert!(rendered.contains("known_flag_typo"));
    assert!(rendered.contains("Raw output artifact:"));
    assert!(rendered.contains("ripgrep"));
}

#[tokio::test]
async fn shell_command_repeated_apply_patch_environment_mismatch_is_suppressed() {
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
        arguments: json!({
            "kind": "argv",
            "program": "apply_patch",
            "args": [patch]
        })
        .to_string(),
    };
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let handler = ShellCommandHandler::default();
    let invoke = |call_id: &str| ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain("shell_command"),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    };

    let first_error = match handler
        .handle(invoke("shell-environment-mismatch-first"))
        .await
    {
        Ok(_) => panic!("the mismatched patch environment must fail verification"),
        Err(error) => error.to_string(),
    };
    assert!(first_error.contains("apply_patch verification failed"));
    assert!(first_error.contains("does not match selected shell environment"));
    assert!(!first_error.contains("execution was suppressed"));

    let second_error = match handler
        .handle(invoke("shell-environment-mismatch-second"))
        .await
    {
        Ok(_) => panic!("the exact repeated environment mismatch must be suppressed"),
        Err(error) => error.to_string(),
    };
    assert!(second_error.contains("apply_patch environment mismatch"));
    assert!(second_error.contains("execution was suppressed"));
}

#[tokio::test]
async fn build_post_tool_use_payload_uses_tool_output_wire_value() {
    let payload = ToolPayload::Function {
        arguments: json!({ "command": "printf shell command" }).to_string(),
    };
    let output = FunctionToolOutput {
        body: vec![],
        canonical_body: None,
        success: Some(true),
        outcome: None,
        post_tool_use_response: Some(json!("shell output")),
        deterministic_continuation_receipts: Vec::new(),
        sampling_request_signal: None,
        deterministic_continuation_owner_key: None,
        skip_disposition: None,
    };
    let handler = ShellCommandHandler::default();
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let mut invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "call-42".to_string(),
        tool_name: codex_tools::ToolName::plain("shell_command"),
        source: ToolCallSource::Direct,
        payload,
    };
    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::shell_command(),
            tool_use_id: "call-42".to_string(),
            tool_input: json!({ "command": "printf shell command" }),
            tool_response: json!("shell output"),
        })
    );

    invocation.payload = ToolPayload::Function {
        arguments: json!({
            "kind": "argv",
            "program": "rg",
            "args": ["--files"]
        })
        .to_string(),
    };
    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::shell_command(),
            tool_use_id: "call-42".to_string(),
            tool_input: json!({
                "command": "rg --files",
                "kind": "argv",
                "program": "rg",
                "args": ["--files"],
            }),
            tool_response: json!("shell output"),
        })
    );
}

#[tokio::test]
async fn shell_command_reduced_output_advertises_exact_retained_artifact() {
    let (session, mut turn) = make_session_and_context().await;
    turn.model_info.truncation_policy =
        codex_protocol::openai_models::TruncationPolicyConfig::bytes(256);
    let input_home = tempfile::tempdir().expect("search input directory");
    let fixture = input_home.path().join("shell-recovery-notice-input.txt");
    let expected = (0..256)
        .map(|i| format!("notice-proof-{i:04} independent retained output bytes\n"))
        .collect::<String>();
    tokio::fs::write(&fixture, expected.as_bytes())
        .await
        .expect("real search input");
    let payload = ToolPayload::Function {
        arguments: json!({
            "kind": "argv",
            "program": "rg",
            "args": ["--no-heading", "--no-filename", "--no-line-number", "--color", "never", ".", fixture.to_string_lossy()]
        }).to_string(),
    };
    let home = turn.config.codex_home.clone();
    let thread_id = session.thread_id.to_string();
    let turn = Arc::new(turn);
    let output = ShellCommandHandler::default()
        .handle(ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "shell-recovery-notice".to_string(),
            tool_name: codex_tools::ToolName::plain("shell_command"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("normal shell handler executes actual search");
    let rendered = output.code_mode_result(&payload).to_string();
    assert!(
        rendered.contains(
            "command output reduced; recover the full retained output with read_tool_output"
        ),
        "{rendered}"
    );
    assert!(rendered.contains("do not rerun the producer"));
    assert!(
        !rendered.contains("notice-proof-0128"),
        "middle output must actually be reduced"
    );
    let artifact_id = rendered
        .split_once("Raw output artifact: ")
        .expect("raw artifact metadata")
        .1
        .split_whitespace()
        .next()
        .expect("artifact ID");
    let retained = crate::tools::command_output_artifact::read_exact_tool_output_artifact(
        &home,
        &thread_id,
        artifact_id,
    )
    .await
    .expect("normal artifact recovery");
    assert_eq!(
        retained,
        expected.as_bytes(),
        "the advertised artifact must recover every producer byte"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn shell_attempt_fingerprint_distinguishes_non_unicode_workdirs() {
    use codex_utils_absolute_path::AbsolutePathBuf;
    use std::os::windows::ffi::OsStringExt;

    let workspace = tempfile::tempdir().unwrap();
    let dirs = [0xd800, 0xd801].map(|code_unit| {
        let path = workspace
            .path()
            .join(std::ffi::OsString::from_wide(&[code_unit]));
        std::fs::create_dir(&path).unwrap();
        AbsolutePathBuf::from_absolute_path(path).unwrap()
    });
    assert_ne!(dirs[0], dirs[1]);
    assert_eq!(dirs[0].to_string_lossy(), dirs[1].to_string_lossy());
    let uris = dirs.each_ref().map(PathUri::from_abs_path);
    assert_ne!(uris[0], uris[1]);
    assert_eq!(uris[0].to_abs_path().unwrap(), dirs[0]);
    assert_eq!(uris[1].to_abs_path().unwrap(), dirs[1]);

    let (session, base_turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let environment = base_turn
        .environments
        .primary()
        .unwrap()
        .environment
        .clone();
    let selected_id = base_turn
        .environments
        .primary()
        .unwrap()
        .environment_id
        .clone();
    let shell = base_turn.environments.primary().unwrap().shell.clone();
    let patch = "*** Begin Patch\n*** Environment ID: deliberately-other-environment\n*** Add File: forbidden.txt\n+must not be written\n*** End Patch";
    let expected_verification_error = format!(
        "apply_patch verification failed: patch environment id `deliberately-other-environment` does not match selected shell environment `{selected_id}`"
    );
    let mut turn = Arc::new(base_turn);
    // Only the cwd changes. The shared session ledger, environment identity, argv and policies stay equal.
    for (index, expected_block) in [(0, false), (0, true), (1, false)] {
        let turn_mut =
            Arc::get_mut(&mut turn).expect("finished rejection releases its turn context");
        turn_mut.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        turn_mut.environments.turn_environments = vec![TurnEnvironment::new(
            selected_id.clone(),
            environment.clone(),
            uris[index].clone(),
            shell.clone(),
        )];
        let error = match ShellCommandHandler::default()
            .handle(ToolInvocation {
                session: session.clone(),
                step_context: StepContext::for_test(turn.clone()),
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: format!("non-unicode-{index}-{expected_block}"),
                tool_name: codex_tools::ToolName::plain("shell_command"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: json!({
                        "kind": "argv", "program": "apply_patch", "args": [patch],
                    })
                    .to_string(),
                },
            })
            .await
        {
            Err(crate::FunctionCallError::RespondToModel(error)) => error,
            Err(error) => panic!("unexpected handler error: {error}"),
            Ok(_) => panic!("mismatched patch environment must reject without writing"),
        };
        if expected_block {
            assert!(
                error.contains("apply_patch environment mismatch"),
                "{error}"
            );
            assert_ne!(
                error, expected_verification_error,
                "an exact retry must use the saved deterministic refusal"
            );
        } else {
            assert_eq!(
                error, expected_verification_error,
                "a distinct native cwd must reach patch verification and cannot reuse another cwd's refusal"
            );
        }
        for dir in &dirs {
            assert!(
                !dir.join("forbidden.txt").exists(),
                "rejected patches cannot mutate either cwd"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_shell_declared_validation_preserves_scope_without_proof() {
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    for (exit_code, tagged, denied) in [
        (0, true, false),
        (7, true, false),
        (0, false, false),
        (0, true, true),
    ] {
        let marker = workspace
            .path()
            .join(format!("shell-{exit_code}-{tagged}-{denied}"));
        let marker_literal = serde_json::to_string(&marker.to_string_lossy()).unwrap();
        let script = format!(
            "import pathlib,sys; pathlib.Path({marker_literal}).write_text('executed'); print('SHELL_ACTUAL_OUTPUT'); sys.exit({exit_code})"
        );
        let (session, mut turn) = make_session_and_context().await;
        turn.approval_policy =
            crate::config::Constrained::allow_any(codex_protocol::protocol::AskForApproval::Never);
        turn.permission_profile = if denied {
            codex_protocol::models::PermissionProfile::Managed {
                file_system: codex_protocol::models::ManagedFileSystemPermissions::Restricted {
                    entries: vec![],
                    glob_scan_max_depth: None,
                },
                network: codex_protocol::permissions::NetworkSandboxPolicy::Restricted,
            }
        } else {
            codex_protocol::models::PermissionProfile::Disabled
        };
        turn.model_info.shell_type =
            codex_protocol::openai_models::ConfigShellToolType::ShellCommand;
        let mut config = (*turn.config).clone();
        config
            .features
            .set_enabled(codex_features::Feature::UnifiedExec, false)
            .unwrap();
        config.permissions.approval_policy =
            crate::config::Constrained::allow_any(codex_protocol::protocol::AskForApproval::Never);
        turn.config = Arc::new(config);
        let session = Arc::new(session);
        let step = StepContext::for_test(Arc::new(turn));
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
            session,
            step,
            Arc::new(Mutex::new(TurnDiffTracker::new())),
        );
        let mut args = json!({"kind": "argv", "program": python, "args": ["-c", script]});
        if tagged {
            args["validation"] = json!({"covered_paths": ["src/declared-shell.rs"]});
        }
        if denied {
            args["sandbox_permissions"] = json!("require_escalated");
            args["justification"] = json!("verify rejection before launch");
        }
        let response = runtime
            .handle_tool_call(
                crate::tools::router::ToolCall {
                    tool_name: codex_tools::ToolName::plain("shell_command"),
                    call_id: "shell-attribution".into(),
                    payload: ToolPayload::Function {
                        arguments: args.to_string(),
                    },
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = response
        else {
            panic!("registered shell result")
        };
        let text = output.body.to_text().unwrap();
        if denied {
            assert_eq!(output.success, Some(false));
            assert!(!marker.exists(), "denied shell must not execute");
        } else {
            assert_eq!(
                tokio::fs::read_to_string(&marker).await.unwrap(),
                "executed"
            );
            assert!(text.contains("SHELL_ACTUAL_OUTPUT"), "{text}");
            assert_eq!(output.success, Some(exit_code == 0), "{text}");
            assert_eq!(text.contains("src/declared-shell.rs"), tagged, "{text}");
            assert_eq!(text.contains("unverified"), tagged, "{text}");
        }
    }
}
