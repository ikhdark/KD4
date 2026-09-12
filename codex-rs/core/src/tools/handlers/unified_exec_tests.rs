use super::exec_command::attach_powershell_failure_advisory;
use super::exec_command::finalize_sandbox_denial_artifact;
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

#[test]
fn exec_command_boundary_normalizes_unambiguous_legacy_forms() {
    let cases = [
        (
            serde_json::json!({"cmd": "rg --files"}),
            CommandInvocation::Script("rg --files".to_string()),
        ),
        (
            serde_json::json!({"program": "rg", "args": ["--files"]}),
            CommandInvocation::Argv {
                program: "rg".to_string(),
                args: vec!["--files".to_string()],
            },
        ),
        (
            serde_json::json!({"script_body": "Get-ChildItem"}),
            CommandInvocation::PowerShellScript("Get-ChildItem".to_string()),
        ),
    ];

    for (arguments, expected) in cases {
        let decoded: ExecCommandArgs =
            parse_arguments(&arguments.to_string()).expect("legacy boundary form should decode");
        assert_eq!(decoded.command_invocation().unwrap(), expected);
    }
}

#[test]
fn exec_command_boundary_accepts_legacy_shell_timeouts_without_changing_yield() {
    let decoded: ExecCommandArgs = parse_arguments(
        &serde_json::json!({
            "cmd": "long-running",
            "timeout_ms": 60_000,
            "stall_timeout_ms": 5_000
        })
        .to_string(),
    )
    .expect("legacy shell timeout fields should remain compatible");

    assert_eq!(decoded.yield_time_ms, default_exec_yield_time_ms());
}

#[test]
fn exec_command_boundary_reports_branch_field_and_bound_errors() {
    let legacy_kind = validate_exec_command_arguments(
        &serde_json::json!({"kind": "legacy", "cmd": "rg --files"}).to_string(),
    )
    .expect_err("legacy kind must be omitted at the compatibility boundary");
    assert!(legacy_kind.contains("$.kind"), "{legacy_kind}");
    assert!(
        legacy_kind.contains("actual value `legacy`"),
        "{legacy_kind}"
    );
    assert!(legacy_kind.contains("omit `kind`"), "{legacy_kind}");

    let mixed_branch = validate_exec_command_arguments(
        &serde_json::json!({
            "kind": "argv",
            "program": "rg",
            "cmd": "rg --files"
        })
        .to_string(),
    )
    .expect_err("mixed command branches must be rejected");
    assert!(mixed_branch.contains("`argv` branch"), "{mixed_branch}");
    assert!(mixed_branch.contains("$.cmd"), "{mixed_branch}");

    let invalid_bound = validate_exec_command_arguments(
        &serde_json::json!({"cmd": "rg --files", "yield_time_ms": 50}).to_string(),
    )
    .expect_err("out-of-range yield must be rejected");
    assert!(invalid_bound.contains("$.yield_time_ms"), "{invalid_bound}");
    assert!(invalid_bound.contains("actual value 50"), "{invalid_bound}");
    assert!(invalid_bound.contains("250..=30000"), "{invalid_bound}");
}

#[tokio::test]
async fn exec_command_cancellation_waits_for_confirmed_process_cleanup() {
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Python is required by the unified-exec cancellation test");
    let temp = tempfile::tempdir().expect("temporary cancellation directory");
    let started_path = temp.path().join("started");
    let finished_path = temp.path().join("finished");
    let started_literal = serde_json::to_string(&started_path.to_string_lossy()).unwrap();
    let finished_literal = serde_json::to_string(&finished_path.to_string_lossy()).unwrap();
    let script = format!(
        "import pathlib,socket,time; listener=socket.socket(); listener.bind(('127.0.0.1',0)); listener.listen(1); pathlib.Path({started_literal}).write_text(str(listener.getsockname()[1])); time.sleep(30); pathlib.Path({finished_literal}).write_text('finished')"
    );
    let program = python.to_string_lossy().into_owned();
    let command = vec![program.clone(), "-c".to_string(), script.clone()];
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(codex_features::Feature::UnifiedExec)
        .expect("enable the normal registered exec tool");
    turn.config = Arc::new(config);
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
        .expect("allow the bounded cancellation test command");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    let invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: cancellation_token.clone(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "cancel-confirmed-cleanup".to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": program,
                "args": ["-c", script],
                "validation": {"covered_paths": ["cancelled-scope"]},
                "yield_time_ms": 20_000
            })
            .to_string(),
        },
    };
    let router = Arc::new(crate::tools::router::ToolRouter::from_context(
        invocation.step_context.as_ref(),
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
    assert!(invocation.step_context.set_tool_router(router).is_ok());
    let runtime = crate::tools::parallel::ToolCallRuntime::new(
        invocation.session,
        invocation.step_context,
        invocation.tracker,
    );
    let task = tokio::spawn(runtime.handle_tool_call(
        crate::tools::router::ToolCall {
            tool_name: invocation.tool_name,
            call_id: invocation.call_id,
            payload: invocation.payload,
        },
        invocation.cancellation_token,
    ));

    let port = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(value) = tokio::fs::read_to_string(&started_path).await
                && let Ok(port) = value.parse::<u16>()
            {
                break port;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test process should publish its bound listener port");
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
        "the started child must still own its live listener"
    );
    cancellation_token.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("cancellation cleanup should be bounded")
        .expect("handler task should join");
    let response = result.expect("registered cancellation returns a terminal function result");
    let codex_protocol::models::ResponseInputItem::FunctionCallOutput { call_id, output } =
        response
    else {
        panic!("registered exec must return function output");
    };
    assert_eq!(call_id, "cancel-confirmed-cleanup");
    let codex_protocol::models::FunctionCallOutputBody::Text(text) = output.body else {
        panic!("cancelled exec output must be text");
    };
    assert!(text.contains("aborted by user"), "{text}");
    assert!(
        !text.contains("\"coverage_status\":\"succeeded\""),
        "{text}"
    );
    let released_listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .expect("the child-owned listener must close before registered cancellation returns");
    drop(released_listener);
    assert!(
        tokio::fs::metadata(&finished_path).await.is_err(),
        "the child must be terminated before cancellation returns"
    );

    let process_id = session
        .services
        .unified_exec_manager
        .allocate_process_id()
        .await;
    assert_eq!(
        process_id, 1000,
        "the cancelled reservation must be released"
    );
    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
}

#[tokio::test]
async fn sandbox_denial_preserves_the_process_raw_output_artifact() {
    let temp = tempfile::tempdir().expect("temporary codex home");
    let pending = crate::tools::command_output_artifact::RawOutputArtifact::pending(
        temp.path(),
        "sandbox-denial",
    );
    let preserved = crate::tools::command_output_artifact::RawOutputArtifact::unavailable(
        "preserved process artifact",
    );

    let finalized = finalize_sandbox_denial_artifact(
        &pending,
        Some(preserved.clone()),
        b"bounded model projection",
    )
    .await;

    assert_eq!(finalized, preserved);
}

#[tokio::test]
async fn unified_pipeline_validation_is_denied_before_process_launch() {
    let (session, turn) = make_session_and_context().await;
    {
        let mut authorization = turn.validation_authorization.write().await;
        *authorization = crate::validation_admission::ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests"));
    }
    let turn = Arc::new(turn);
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "script",
            "cmd": "cargo test | cargo --version"
        })
        .to_string(),
    };

    let (output, launches) =
        crate::tools::runtimes::unified_exec::test_observation::observe(async {
            ExecCommandHandler::default()
                .handle(ToolInvocation {
                    session: session.into(),
                    step_context: StepContext::for_test(turn),
                    cancellation_token: tokio_util::sync::CancellationToken::new(),
                    tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                    call_id: "unified-pipeline-validation-denied".to_string(),
                    tool_name: codex_tools::ToolName::plain("exec_command"),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
        })
        .await;
    let output = output.expect("the denied pipeline should return a structured skip");
    let structured = output
        .post_tool_use_response("unified-pipeline-validation-denied", &payload)
        .expect("the validation skip should retain its structured result");

    assert_eq!(structured["reason"], "user_prohibited_validation");
    assert_eq!(structured["operation"], "test");
    assert_eq!(structured["command_was_executed"], false);
    assert_eq!(launches.process_launches, 0);
}

#[tokio::test]
async fn late_unified_validation_denial_records_suppressed_timing() {
    let (_session, turn) = make_session_and_context().await;
    let invocation = CommandInvocation::Argv {
        program: "cargo".to_string(),
        args: vec!["test".to_string()],
    };
    let skipped = {
        let mut authorization = turn.validation_authorization.write().await;
        *authorization = crate::validation_admission::ValidationAuthorization::enabled();
        assert!(authorization.update_from_user_input("do not run tests"));
        crate::validation_admission::prohibited_skip_for(&authorization, &invocation, true)
            .expect("test denial suppresses the validation")
    };

    super::exec_command::record_late_validation_skip(&turn, &skipped);

    assert_eq!(
        turn.turn_timing_state
            .complete_snapshot()
            .protocol_timing()
            .counters
            .suppressed_validation_output_count,
        1,
    );
}

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
        validation: None,
        event_call_id: "call-parser-failure".to_string(),
        chunk_id: "chunk-parser-failure".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: raw_output.clone(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(1),
        process_exited: true,
        original_token_count: None,
        hook_command: Some("broken command".to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
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
async fn identical_tagged_validation_rg_misses_both_launch() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    {
        let mut authorization = turn.validation_authorization.write().await;
        *authorization = crate::validation_admission::ValidationAuthorization::enabled();
    }
    let validation_repository = tempfile::tempdir().expect("temporary validation repository");
    let git_init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(validation_repository.path())
        .output()
        .expect("initialize validation repository");
    assert!(git_init.status.success());
    let search_target = validation_repository.path().join("search-target.txt");
    tokio::fs::write(&search_target, "present\n")
        .await
        .expect("write validation search target");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": "rg",
            "args": [
                "-n",
                "__codex_validation_unmatched_probe__",
                search_target.clone(),
            ],
            "validation": {
                "covered_paths": [search_target],
            },
            "workdir": validation_repository.path(),
            "yield_time_ms": 10_000,
        })
        .to_string(),
    };

    let ((first, second), launches) =
        crate::tools::runtimes::unified_exec::test_observation::observe(async {
            let first = run_exec_command_for_test(
                &session,
                &turn,
                "validation-first-miss",
                payload.clone(),
            )
            .await;
            let second = run_exec_command_for_test(
                &session,
                &turn,
                "validation-second-miss",
                payload.clone(),
            )
            .await;
            (first, second)
        })
        .await;

    assert_eq!(launches.process_launches, 2);
    assert_eq!(first.code_mode_result(&payload)["exit_code"], 1);
    assert_eq!(second.code_mode_result(&payload)["exit_code"], 1);
    let counters = turn
        .turn_timing_state
        .complete_snapshot()
        .protocol_timing()
        .counters;
    assert_eq!(
        counters.executed_validation_count, 2,
        "every launched validation must publish exactly one result",
    );
    assert!(
        counters.executed_validation_duration_ns > 0,
        "failed validations must retain their measured execution duration",
    );
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
        .args(["rev-parse", "HEAD:codex-rs/core/src/lib.rs"])
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
async fn registered_exec_minimal_and_explicit_defaults_preserve_process_and_permissions() {
    async fn dispatch(
        invocation: ToolInvocation,
    ) -> codex_protocol::models::FunctionCallOutputPayload {
        let router = Arc::new(crate::tools::router::ToolRouter::from_context(
            invocation.step_context.as_ref(),
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
        assert!(invocation.step_context.set_tool_router(router).is_ok());
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            invocation.session,
            invocation.step_context,
            invocation.tracker,
        );
        let response = runtime
            .handle_tool_call(
                crate::tools::router::ToolCall {
                    tool_name: invocation.tool_name,
                    call_id: invocation.call_id,
                    payload: invocation.payload,
                },
                invocation.cancellation_token,
            )
            .await
            .expect("registered exec returns");
        let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = response
        else {
            panic!("exec must return a function result");
        };
        output
    }

    let workspace = tempfile::tempdir().expect("command workspace");
    let cwd = workspace.path().join("exact cwd");
    std::fs::create_dir(&cwd).expect("explicit working directory");
    for allow_login in [false, true] {
        let (session, mut turn) = make_session_and_context().await;
        turn.permission_profile = PermissionProfile::Disabled;
        let mut config = (*turn.config).clone();
        config
            .features
            .enable(codex_features::Feature::UnifiedExec)
            .expect("enable exec");
        config.permissions.allow_login_shell = allow_login;
        config.permissions.approval_policy =
            crate::config::Constrained::allow_any(codex_protocol::protocol::AskForApproval::Never);
        config
            .permissions
            .shell_environment_policy
            .set
            .insert("KD4_ARGUMENT_PROOF".into(), "configured value".into());
        turn.config = Arc::new(config);
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let exit_code = if allow_login { 7 } else { 0 };
        let script = format!(
            "import os,sys,json; print('PROOF='+json.dumps(dict(args=sys.argv[1:],cwd=os.path.basename(os.getcwd()),env=os.environ.get('KD4_ARGUMENT_PROOF')))); sys.exit({exit_code})"
        );
        for explicit in [false, true] {
            let mut args = serde_json::json!({
                "program": "python", "args": ["-c", script, "two words", "$literal", "a\"b", ""],
                "workdir": cwd,
            });
            if explicit {
                args.as_object_mut().unwrap().extend(
                    serde_json::json!({
                        "kind": "argv", "tty": false, "force_fresh": false,
                        "yield_time_ms": 2000, "login": allow_login,
                        "sandbox_permissions": "use_default", "additional_permissions": null,
                        "justification": null, "prefix_rule": null, "validation": null,
                        "max_output_tokens": null, "environment_id": null,
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                );
            }
            let output = dispatch(ToolInvocation {
                session: session.clone(),
                step_context: StepContext::for_test(turn.clone()),
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: format!("defaults-{allow_login}-{explicit}"),
                tool_name: codex_tools::ToolName::plain("exec_command"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: args.to_string(),
                },
            })
            .await;
            let text = output.body.to_text().expect("process output");
            assert!(
                text.contains(&format!("Process exited with code {exit_code}")),
                "{text}"
            );
            let proof = text
                .lines()
                .find_map(|line| line.strip_prefix("PROOF="))
                .expect("child process proof");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(proof).unwrap(),
                serde_json::json!({
                    "args": ["two words", "$literal", "a\"b", ""], "cwd": "exact cwd", "env": "configured value"
                })
            );
        }
        // An explicit permission override remains meaningful: it cannot execute under Never.
        let output = dispatch(ToolInvocation {
            session, step_context: StepContext::for_test(turn),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: format!("denied-{allow_login}"),
            tool_name: codex_tools::ToolName::plain("exec_command"), source: ToolCallSource::Direct,
            payload: ToolPayload::Function { arguments: serde_json::json!({
                "program": "python", "args": ["-c", "open('forbidden', 'w').write('launched')"],
                "workdir": cwd, "sandbox_permissions": "require_escalated", "justification": "test rejection"
            }).to_string() },
        }).await;
        assert_eq!(output.success, Some(false));
        assert!(output.body.to_text().unwrap().contains("approval policy"));
        assert!(
            !cwd.join("forbidden").exists(),
            "denied command must not launch"
        );
    }
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
    assert!(model_output.contains(
        "[command output reduced; recover the full retained output with read_tool_output"
    ));
    let response = output.to_response_item(
        "full-output-artifact",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    let rendered = serde_json::to_string(&response).expect("model response");
    assert!(rendered.contains(
        "[command output reduced; recover the full retained output with read_tool_output"
    ));
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
        validation: None,
        event_call_id: "call-43".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
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
        validation: None,
        event_call_id: "call-44".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
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
        validation: None,
        event_call_id: "event-45".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: Some(45),
        exit_code: None,
        process_exited: false,
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
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
        validation: None,
        event_call_id: "exec-call-45".to_string(),
        chunk_id: "chunk-2".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"finished\n".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: None,
        hook_command: Some("sleep 1; echo finished".to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
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
        validation: None,
        event_call_id: "exec-call-a".to_string(),
        chunk_id: "chunk-a".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"alpha\n".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: None,
        hook_command: Some("sleep 2; echo alpha".to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
        repair_notice: None,
    };
    let output_b = ExecCommandToolOutput {
        validation: None,
        event_call_id: "exec-call-b".to_string(),
        chunk_id: "chunk-b".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"beta\n".to_vec(),
        truncation_policy: TEST_TRUNCATION_POLICY,
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: None,
        hook_command: Some("sleep 1; echo beta".to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
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

async fn assert_completed_exec_reports_tool_history_failure(background: bool) {
    use codex_protocol::items::CommandExecutionStatus;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::EventMsg;
    use std::time::Duration;

    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Python is required by the unified-exec behavior test");
    let workspace = tempfile::tempdir().expect("temporary command workspace");
    let changed_path = workspace.path().join("changed.txt");
    let path_literal = serde_json::to_string(&changed_path.to_string_lossy()).unwrap();
    let wait_for_stdin = if background {
        "line = sys.stdin.readline(); assert line == 'finish\\n', repr(line); "
    } else {
        ""
    };
    let script = format!(
        "import sys,pathlib; {wait_for_stdin}pathlib.Path({path_literal}).write_text('after'); print('MUTATION_FINISHED', flush=True)"
    );
    let program = python.to_string_lossy().into_owned();
    let (session, mut turn, rx_event) = make_session_and_context_with_rx().await;
    Arc::get_mut(&mut turn)
        .expect("single turn")
        .permission_profile = PermissionProfile::Disabled;
    let codex_home = &turn.config.codex_home;
    tokio::fs::create_dir_all(codex_home)
        .await
        .expect("create test home");
    session
        .services
        .exec_policy
        .append_amendment_and_update(
            codex_home,
            &codex_protocol::protocol::ExecPolicyAmendment::new(vec![
                program.clone(),
                "-c".to_string(),
                script.clone(),
            ]),
        )
        .await
        .expect("allow exact bounded test command");
    let observation = crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
        None,
        &ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "prior-workspace-read".to_string(),
            output: FunctionCallOutputPayload::from_text("before mutation".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        Default::default(),
    )
    .expect("workspace observation");
    session
        .register_workspace_evidence(codex_home, observation, ())
        .await;
    session
        .flush_tool_history_persistence()
        .await
        .expect("initial evidence is durable");
    let directory = codex_home.join("tool-history");
    let saved = codex_home.join("saved-tool-history");
    tokio::fs::rename(&directory, &saved)
        .await
        .expect("save baseline");
    tokio::fs::write(&directory, "blocks journal and checkpoint writes")
        .await
        .expect("real failure fixture");

    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv", "program": program, "args": ["-c", script],
            "workdir": workspace.path(), "yield_time_ms": if background { 250 } else { 20_000 },
            "tty": background
        })
        .to_string(),
    };
    let invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::clone(&tracker),
        call_id: "durability-command".to_string(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    };
    let initial_result = tokio::time::timeout(
        Duration::from_secs(15),
        ExecCommandHandler::default().handle(invocation),
    )
    .await
    .expect("foreground completion or live background yield is bounded");
    let result = if background {
        let output =
            initial_result.expect("a still-running process must not wait for terminal durability");
        let process_id = output.code_mode_result(&payload)["session_id"]
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .expect("live process id");
        assert!(
            !changed_path.exists(),
            "the command must still be waiting for stdin"
        );
        assert!(
            session
                .services
                .command_execution
                .running_process(process_id)
                .await
                .is_some(),
            "the initial yield must retain a live command before stdin releases it"
        );
        tokio::time::timeout(Duration::from_secs(15), WriteStdinHandler.handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::clone(&tracker),
            call_id: "durability-stdin".to_string(),
            tool_name: codex_tools::ToolName::plain("write_stdin"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: serde_json::json!({"session_id": process_id, "chars": "finish\n", "yield_time_ms": 1000}).to_string(),
            },
        })).await.expect("exited process must finish terminal persistence and cleanup")
    } else {
        initial_result
    };
    let error = match result {
        Ok(_) => panic!("completed mutation must return fatal persistence failure"),
        Err(error) => error,
    };
    assert!(matches!(error, crate::FunctionCallError::Fatal(_)));
    assert_eq!(
        tokio::fs::read_to_string(&changed_path).await.unwrap(),
        "after"
    );
    assert_eq!(tracker.lock().await.current_mutation_revision(), 1);

    let events: Vec<_> = std::iter::from_fn(|| rx_event.try_recv().ok()).collect();
    let (completed_index, completed) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match &event.msg {
            EventMsg::ItemCompleted(event) => match &event.item {
                TurnItem::CommandExecution(item) if item.id == "durability-command" => {
                    Some((index, item))
                }
                _ => None,
            },
            _ => None,
        })
        .expect("the actual command retains its completed event");
    assert_eq!(completed.status, CommandExecutionStatus::Completed);
    assert_eq!(completed.exit_code, Some(0));
    assert!(
        completed
            .aggregated_output
            .as_deref()
            .unwrap_or_default()
            .contains("MUTATION_FINISHED")
    );
    let process_id = completed
        .process_id
        .as_deref()
        .and_then(|id| id.parse::<u32>().ok())
        .expect("normal command process id");
    assert!(
        session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .is_none(),
        "fatal persistence failure must not skip process retirement"
    );
    let next_id = session
        .services
        .unified_exec_manager
        .allocate_process_id()
        .await;
    assert_eq!(
        next_id, process_id,
        "completed command must release the reserved process slot"
    );
    session
        .services
        .unified_exec_manager
        .release_process_id(next_id)
        .await;
    if background {
        let error_index = events
            .iter()
            .position(|event| matches!(event.msg, EventMsg::Error(_)))
            .expect("background durability failure is also notified");
        assert!(completed_index < error_index);
        assert!(events.iter().any(|event| matches!(&event.msg,
            EventMsg::TerminalInteraction(interaction) if interaction.call_id == "durability-command" && interaction.stdin == "finish\n")),
            "stdin was delivered even though completion durability failed");
    }
    let history = session.clone_history().await;
    let live = serde_json::to_value(history.tool_history_state()).unwrap();
    assert_eq!(
        live["workspace_evidence"]["prior-workspace-read"]["source_dependencies_current"],
        false
    );
    tokio::fs::remove_file(&directory)
        .await
        .expect("remove failure fixture");
    tokio::fs::rename(&saved, &directory)
        .await
        .expect("restore baseline");
    session
        .flush_tool_history_persistence()
        .await
        .expect("recover after repairing storage");
}

#[tokio::test]
async fn completed_exec_returns_fatal_after_tool_history_failure_and_process_cleanup() {
    assert_completed_exec_reports_tool_history_failure(false).await;
}

#[tokio::test]
async fn background_stdin_completion_returns_fatal_after_tool_history_failure_and_process_cleanup()
{
    assert_completed_exec_reports_tool_history_failure(true).await;
}

#[tokio::test]
async fn stdin_completion_prepares_recovery_notice_for_both_output_consumers() {
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .expect("Python is required by the KD4 test environment");
    let program = python.to_string_lossy().into_owned();
    let script = "import sys; line = sys.stdin.readline(); assert line == 'go\\n', repr(line); sys.stdout.write(''.join('stdin-notice-%04d retained producer bytes\\n' % i for i in range(256))); sys.stdout.flush()";
    let expected = (0..256)
        .map(|i| format!("stdin-notice-{i:04} retained producer bytes\n"))
        .collect::<String>();
    let (session, turn) = make_session_and_context().await;
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home");
    session
        .services
        .exec_policy
        .append_amendment_and_update(
            &turn.config.codex_home,
            &codex_protocol::protocol::ExecPolicyAmendment::new(vec![
                program.clone(),
                "-u".to_string(),
                "-c".to_string(),
                script.to_string(),
            ]),
        )
        .await
        .expect("allow exact test producer");
    let home = turn.config.codex_home.clone();
    let thread_id = session.thread_id.to_string();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let invoke = |tool: &str, arguments: serde_json::Value| ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: format!("notice-{tool}"),
        tool_name: codex_tools::ToolName::plain(tool),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    };
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let started = ExecCommandHandler::default()
        .handle(invoke(
            "exec_command",
            serde_json::json!({
                "kind": "argv", "program": program, "args": ["-u", "-c", script],
                "yield_time_ms": 1000, "max_output_tokens": 100, "tty": true
            }),
        ))
        .await
        .expect("normal stdin-waiting process");
    let start_json = started.code_mode_result(&payload);
    let session_id = start_json["session_id"]
        .as_u64()
        .expect("process waits for input");
    let completed = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut chars = "go\n";
        loop {
            let output = WriteStdinHandler
                .handle(invoke(
                    "write_stdin",
                    serde_json::json!({
                        "session_id": session_id, "chars": chars,
                        "yield_time_ms": 1000, "max_output_tokens": 100
                    }),
                ))
                .await
                .expect("normal write_stdin completion");
            if output
                .code_mode_result(&payload)
                .get("session_id")
                .is_none()
            {
                break output;
            }
            chars = "";
        }
    })
    .await
    .expect("producer completes");
    let code_mode = completed.code_mode_result(&payload);
    assert_eq!(code_mode["exit_code"], 0);
    let text = code_mode["output"].as_str().expect("code-mode output");
    assert!(
        text.contains(
            "[command output reduced; recover the full retained output with read_tool_output"
        ),
        "{text}"
    );
    assert!(!text.contains("stdin-notice-0128"));
    let response =
        serde_json::to_string(&completed.to_response_item("notice-write_stdin", &payload))
            .expect("model response");
    assert!(response.contains(
        "[command output reduced; recover the full retained output with read_tool_output"
    ));
    let id = code_mode["raw_output_artifact_id"]
        .as_str()
        .expect("advertised artifact");
    assert!(
        response.contains(id),
        "model recovery notice must retain its artifact ID"
    );
    let retained = crate::tools::command_output_artifact::read_exact_tool_output_artifact(
        &home, &thread_id, id,
    )
    .await
    .expect("normal exact recovery");
    let retained = String::from_utf8(retained).expect("UTF-8 terminal output");
    // A PTY may include input echo, CRLFs and terminal control sequences.
    // Require every independently authored producer record exactly once and in
    // order from the advertised artifact, including the omitted middle records.
    let record = regex_lite::Regex::new(r"stdin-notice-[0-9]{4} retained producer bytes")
        .expect("producer record pattern");
    let recovered_records = record
        .find_iter(&retained)
        .map(|matched| format!("{}\n", matched.as_str()))
        .collect::<String>();
    assert_eq!(recovered_records, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_exec_preserves_foreign_grant_with_explicit_network_request() {
    use codex_protocol::models::FileSystemPermissions;
    use codex_protocol::models::ManagedFileSystemPermissions;
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ReviewDecision;
    use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
    use codex_utils_path_uri::PathUri;
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    // The stored grant belongs to the opposite path convention from this host.
    let grant_root = PathUri::parse(if cfg!(windows) {
        "file:///srv/remote-output"
    } else {
        "file:///C:/remote-output"
    })
    .unwrap();
    let granted = UriAdditionalPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            None,
            Some(vec![grant_root]),
        )),
        network: None,
    };
    assert!(
        codex_protocol::models::AdditionalPermissionProfile::try_from(granted.clone()).is_err()
    );
    for tool_name in ["exec_command", "shell_command"] {
        for (feature_enabled, policy, rejection) in [
            (true, AskForApproval::OnRequest, None),
            (
                false,
                AskForApproval::OnRequest,
                Some("additional permissions are disabled"),
            ),
            (true, AskForApproval::Never, Some("approval policy")),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
            let (read_release, read_release_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted.unwrap(),
                    _ = &mut stop_rx => return Vec::new(),
                };
                let mut socket = tokio_tungstenite::accept_async(accepted.0).await.unwrap();
                let mut starts = Vec::new();
                let mut read_release_rx = Some(read_release_rx);
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
                    let result = match message["method"].as_str().unwrap() {
                        "initialize" => json!({"sessionId": "uri-permission-session"}),
                        "initialized" => continue,
                        "environment/info" => json!({
                            "operatingSystem": "windows",
                            "shell": {"name": "cmd", "path": "cmd.exe"},
                            "cwd": "file:///C:/remote-output"
                        }),
                        "process/start" => {
                            starts.push(message["params"].clone());
                            json!({"processId": message["params"]["processId"]})
                        }
                        "process/read" => json!({
                            "chunks": [{"stream": "stdout", "chunk": "cmVtb3RlLXVyaS1wcm9vZgo=", "seq": 1}],
                            "nextSeq": 4, "exited": true, "exitCode": 7,
                            "closed": true, "failure": null, "sandboxDenied": false
                        }),
                        "process/terminate" => json!({}),
                        method => panic!("unexpected executor operation {method}: {message}"),
                    };
                    socket
                        .send(Message::Text(
                            json!({"id": message["id"], "result": result})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    if message["method"] == "process/start" {
                        read_release_rx
                            .take()
                            .expect("one remote process")
                            .await
                            .expect("test releases remote output after the real yielded result");
                        // Normal live exec-server output uses notifications. process/read
                        // is recovery, so a read-only peer would never settle a live process.
                        let process_id = &message["params"]["processId"];
                        for notification in [
                            json!({"method": "process/output", "params": {"processId": process_id, "seq": 1, "stream": "stdout", "chunk": "cmVtb3RlLXVyaS1wcm9vZgo="}}),
                            json!({"method": "process/exited", "params": {"processId": process_id, "seq": 2, "exitCode": 7, "sandboxDenied": false}}),
                            json!({"method": "process/closed", "params": {"processId": process_id, "seq": 3}}),
                        ] {
                            socket
                                .send(Message::Text(notification.to_string().into()))
                                .await
                                .unwrap();
                        }
                    }
                }
                starts
            });
            let home = tempfile::tempdir().unwrap();
            let (session, mut turn, events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    codex_login::CodexAuth::from_api_key("Test API Key"),
                    Vec::new(),
                    home.path(),
                    |config| {
                        config
                            .features
                            .enable(codex_features::Feature::UnifiedExec)
                            .unwrap();
                        config
                            .features
                            .set_enabled(
                                codex_features::Feature::ExecPermissionApprovals,
                                feature_enabled,
                            )
                            .unwrap();
                        if tool_name == "shell_command" {
                            config
                                .features
                                .set_enabled(codex_features::Feature::UnifiedExec, false)
                                .unwrap();
                        }
                        config.permissions.approval_policy =
                            crate::config::Constrained::allow_any(policy);
                        config
                            .permissions
                            .set_permission_profile(PermissionProfile::Managed {
                                file_system: ManagedFileSystemPermissions::Restricted {
                                    entries: vec![],
                                    glob_scan_max_depth: None,
                                },
                                network: NetworkSandboxPolicy::Restricted,
                            })
                            .unwrap();
                    },
                )
                .await;
            let environment = Arc::new(Environment::create_for_tests(Some(url)).unwrap());
            let scope = environment.approval_scope_id().to_string();
            let turn_mut =
                Arc::get_mut(&mut turn).expect("fixture owns the turn before registration");
            let cwd = if tool_name == "shell_command" {
                turn_mut.model_info.shell_type =
                    codex_protocol::openai_models::ConfigShellToolType::ShellCommand;
                PathUri::parse(if cfg!(windows) {
                    "file:///srv/remote-output"
                } else {
                    "file:///C:/remote-output"
                })
                .unwrap()
            } else {
                turn_mut.cwd_uri()
            };
            turn_mut.environments.turn_environments =
                vec![crate::session::turn_context::TurnEnvironment::new(
                    "grant-remote".into(),
                    environment,
                    cwd.clone(),
                    None,
                )];
            let active = crate::state::ActiveTurn::default();
            active
                .turn_state
                .lock()
                .await
                .record_granted_permissions(&scope, granted.clone());
            *session.active_turn.lock().await = Some(active);
            assert_eq!(
                session.granted_turn_permissions(&scope).await,
                Some(granted.clone())
            );
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
            let call = runtime.clone().handle_tool_call(
            crate::tools::router::ToolCall {
                tool_name: codex_tools::ToolName::plain(tool_name),
                call_id: "uri-grant-exec".into(),
                payload: ToolPayload::Function {
                    arguments: {
                        let mut args = json!({
                            "kind": "argv", "program": "uri-proof-command", "args": ["two words"],
                            "sandbox_permissions": "with_additional_permissions",
                            "additional_permissions": {"network": {"enabled": true}},
                            "validation": {"covered_paths": ["remote-declared-scope"]},
                        });
                        if tool_name == "exec_command" { args["yield_time_ms"] = json!(1000); }
                        args.to_string()
                    },
                },
            },
            tokio_util::sync::CancellationToken::new(),
        );
            tokio::pin!(call);
            let response = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                tokio::select! {
                    result = &mut call => break result.unwrap(),
                    event = events.recv() => {
                        if let EventMsg::ExecApprovalRequest(request) = event.unwrap().msg {
                            assert!(rejection.is_none(), "rejected request must not prompt for execution");
                            session.notify_approval(request.approval_id.as_deref().unwrap_or(&request.call_id), ReviewDecision::Approved).await;
                        }
                    }
                }
            }
        }).await.expect("registered execution must complete");
            let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } =
                response
            else {
                panic!("registered exec must return a function output");
            };
            let mut text = output.body.to_text().unwrap();
            if rejection.is_none() {
                assert!(
                    text.contains("Process running with session ID "),
                    "remote output is intentionally pending: {text}"
                );
                read_release
                    .send(())
                    .expect("remote output consumer remains owned");
                text = tokio::time::timeout(Duration::from_secs(15), async {
                    while let Some((_, status)) = text.split_once("Process running with session ID ") {
                        assert!(text.contains("remote-declared-scope") && text.contains("unverified"), "running result retains attribution: {text}");
                        let id = status.split(';').next().unwrap().parse::<u32>().unwrap();
                        let response = runtime.clone().handle_tool_call(
                            crate::tools::router::ToolCall {
                                tool_name: codex_tools::ToolName::plain("write_stdin"),
                                call_id: "uri-grant-poll".into(),
                                payload: ToolPayload::Function {
                                    arguments: json!({"session_id": id, "chars": "", "yield_time_ms": 1000}).to_string(),
                                },
                            },
                            tokio_util::sync::CancellationToken::new(),
                        ).await.unwrap();
                        let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = response else { panic!("registered remote poll output") };
                        text = output.body.to_text().unwrap();
                    }
                    text
                }).await.expect("remote process must settle through normal registered polling");
            }
            // The client may already have closed the external socket after settlement.
            let _ = stop_tx.send(());
            let starts = tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            if let Some(rejection) = rejection {
                assert_eq!(output.success, Some(false));
                assert!(text.contains(rejection), "{text}");
                assert!(
                    !text.contains("\"coverage_status\":\"succeeded\""),
                    "{text}"
                );
                assert!(
                    starts.is_empty(),
                    "rejected permissions must never launch remotely"
                );
            } else {
                assert!(text.contains("Process exited with code 7"), "{text}");
                assert!(text.contains("remote-uri-proof"), "{text}");
                assert!(text.contains("remote-declared-scope"), "{text}");
                assert!(text.contains("unverified"), "{text}");
                assert_eq!(starts.len(), 1, "exactly one remote launch");
                assert_eq!(starts[0]["argv"], json!(["uri-proof-command", "two words"]));
                assert_eq!(starts[0]["cwd"], json!(cwd));
                let expected = PermissionProfile::Managed {
                    file_system: ManagedFileSystemPermissions::Restricted {
                        entries: granted.file_system.as_ref().unwrap().entries.clone(),
                        glob_scan_max_depth: None,
                    },
                    network: NetworkSandboxPolicy::Enabled,
                };
                assert_eq!(
                    starts[0]["sandbox"]["permissions"],
                    serde_json::to_value(expected).unwrap(),
                    "remote sandbox must retain the entire foreign grant and explicit network permission"
                );
            }
            assert_eq!(
                session.granted_turn_permissions(&scope).await,
                Some(granted.clone()),
                "a per-command network request must not overwrite or widen stored grants"
            );
        }
    }
}

#[cfg(windows)]
#[test]
fn registered_shell_analysis_yields_and_preserves_search_results() {
    use std::time::Duration;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker tool runtime");
    runtime.block_on(async {
        for tool_name in ["shell_command", "exec_command"] {
            for scenario in ["original", "repaired", "denied", "mutating", "script_typo"] {
                let repaired = scenario == "repaired";
                let denied = scenario == "denied";
                let mutating = scenario == "mutating";
                let script_typo = scenario == "script_typo";
                let workspace = tempfile::tempdir().expect("selected command cwd");
                std::fs::write(
                    workspace.path().join("input.txt"),
                    "unrelated\nneedle-worker-proof\n",
                )
                .expect("independent search fixture");
                let (session, mut turn, events) = make_session_and_context_with_rx().await;
                let turn_mut = Arc::get_mut(&mut turn).expect("unshared setup turn");
                turn_mut.permission_profile = PermissionProfile::Disabled;
                turn_mut.session_source = if mutating {
                    codex_protocol::protocol::SessionSource::Cli
                } else {
                    codex_protocol::protocol::SessionSource::SubAgent(
                        codex_protocol::protocol::SubAgentSource::Review,
                    )
                };
                let mut config = (*turn_mut.config).clone();
                config.features.enable(codex_features::Feature::UnifiedExec).unwrap();
                config.features.disable(codex_features::Feature::DirectRuntime).unwrap();
                config.permissions.allow_login_shell = false;
                config.permissions.approval_policy = crate::config::Constrained::allow_any(
                    codex_protocol::protocol::AskForApproval::Never,
                );
                turn_mut.config = Arc::new(config);
                let selected = turn_mut.environments.turn_environments.first_mut().unwrap();
                // Explicit PowerShell input must discover PowerShell instead of running in Cmd.
                selected.shell = Some(crate::shell::Shell {
                    shell_type: ShellType::Cmd,
                    shell_path: PathBuf::from("cmd.exe"),
                });
                let step = StepContext::for_test(Arc::clone(&turn));
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
                let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
                let tool_runtime = crate::tools::parallel::ToolCallRuntime::new(
                    session,
                    step,
                    Arc::clone(&tracker),
                );
                let call_id = format!("worker-search-{tool_name}-{scenario}");
                let script = if denied {
                    "Set-Content -LiteralPath forbidden.txt -Value launched".to_string()
                } else if mutating {
                    "Set-Content -LiteralPath mutation.txt -NoNewline -Value 'mutation-worker-proof'; Get-Content -LiteralPath mutation.txt".to_string()
                } else if script_typo {
                    "rg --ignorecase --color never -n needle input.txt; Set-Content -LiteralPath forbidden.txt -Value launched".to_string()
                } else {
                    "rg --ignore-case --color never -n needle input.txt".to_string()
                };
                // Only direct argv has the execution-safe equivalent repair contract.
                let arguments = if repaired {
                    serde_json::json!({
                        "kind": "argv", "program": "rg",
                        "args": ["--ignorecase", "--color", "never", "-n", "needle", "input.txt"],
                        "workdir": workspace.path(), "login": false,
                    })
                } else {
                    serde_json::json!({
                        "kind": "powershell_script", "script_body": script,
                        "workdir": workspace.path(), "login": false,
                    })
                };
                let payload = ToolPayload::Function { arguments: arguments.to_string() };
                let (occupied_tx, occupied_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    occupied_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).is_ok()
                });
                occupied_rx.await.unwrap();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let request_call_id = call_id.clone();
                let request = tokio::spawn(async move {
                    entered_tx.send(()).unwrap();
                    tool_runtime.handle_tool_call(
                        crate::tools::router::ToolCall {
                            tool_name: codex_tools::ToolName::plain(tool_name),
                            call_id: request_call_id,
                            payload,
                        },
                        tokio_util::sync::CancellationToken::new(),
                    ).await
                });
                entered_rx.await.unwrap();
                tokio::time::sleep(Duration::from_millis(25)).await;
                assert!(!blocker.is_finished(), "timer progresses while the worker is occupied");
                assert!(!request.is_finished(), "registered tool awaits analysis before execution");
                while let Ok(event) = events.try_recv() {
                    assert!(
                        !matches!(event.msg, codex_protocol::protocol::EventMsg::ExecCommandBegin(ref event) if event.call_id == call_id),
                        "queued analysis must not publish command execution",
                    );
                }
                release_tx.send(()).unwrap();
                assert!(blocker.await.unwrap());
                let response = tokio::time::timeout(Duration::from_secs(30), request)
                    .await.expect("normal PowerShell search finishes")
                    .expect("tool task").expect("registered tool response");
                let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = response else {
                    panic!("registered shell tool must return a function result");
                };
                let text = output.body.to_text().expect("command output");
                if denied || script_typo {
                    assert_eq!(output.success, Some(false));
                    let expected_error = if script_typo {
                        "known_flag_typo"
                    } else {
                        "independent reviewers may run only shell commands proven read-only"
                    };
                    assert!(text.contains(expected_error), "{text}");
                    assert!(!workspace.path().join("forbidden.txt").exists(), "rejected mutation must not execute");
                    while let Ok(event) = events.try_recv() {
                        assert!(
                            !matches!(event.msg, codex_protocol::protocol::EventMsg::ExecCommandBegin(ref event) if event.call_id == call_id),
                            "rejected mutation must not publish command execution",
                        );
                    }
                } else {
                    if mutating {
                        assert!(text.contains("mutation-worker-proof"), "{text}");
                        assert_eq!(std::fs::read_to_string(workspace.path().join("mutation.txt")).unwrap(), "mutation-worker-proof");
                    } else {
                        assert!(text.contains("2:needle-worker-proof"), "{text}");
                    }
                    let expected_exit = if tool_name == "shell_command" { "Exit code: 0" } else { "Process exited with code 0" };
                    assert!(text.contains(expected_exit), "{text}");
                    if repaired {
                        assert!(text.contains("known_flag_typo"), "{text}");
                    }
                }
                assert_eq!(tracker.lock().await.current_mutation_revision(), u64::from(mutating), "only an executed mutation advances the turn revision");
                assert_eq!(
                    std::fs::read_to_string(workspace.path().join("input.txt")).unwrap(),
                    "unrelated\nneedle-worker-proof\n",
                    "read-only search preserves its selected input",
                );
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_exec_declared_validation_survives_yield_and_stdin_completion() {
    use serde_json::json;
    use std::time::Duration;
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let release = workspace.path().join("release");
    let release_literal = serde_json::to_string(&release.to_string_lossy()).unwrap();
    let script = format!(
        "import pathlib,time; print('CHILD_STARTED',flush=True); p=pathlib.Path({release_literal}); exec('while not p.exists(): time.sleep(0.01)'); print('CHILD_FINISHED',flush=True)"
    );
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(codex_features::Feature::UnifiedExec)
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
        session.clone(),
        step,
        Arc::new(Mutex::new(TurnDiffTracker::new())),
    );
    let initial = runtime.clone().handle_tool_call(crate::tools::router::ToolCall {
        tool_name: codex_tools::ToolName::plain("exec_command"), call_id: "declared-validation-child".into(),
        payload: ToolPayload::Function { arguments: json!({
            "kind": "argv", "program": python, "args": ["-c", script], "yield_time_ms": 1000,
            "validation": {"covered_paths": ["src/declared-only.rs", "tests/declared-only.rs"]}
        }).to_string() },
    }, tokio_util::sync::CancellationToken::new()).await.unwrap();
    let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = initial
    else {
        panic!("normal exec output")
    };
    let text = output.body.to_text().unwrap();
    assert!(text.contains("CHILD_STARTED"), "{text}");
    assert!(
        text.contains("src/declared-only.rs") && text.contains("tests/declared-only.rs"),
        "{text}"
    );
    assert!(text.contains("unverified"), "{text}");
    let id = text
        .split_once("Process running with session ID ")
        .expect("child must yield")
        .1
        .split(';')
        .next()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    tokio::fs::write(&release, b"finish").await.unwrap();
    let settled = tokio::time::timeout(
        Duration::from_secs(15),
        runtime.clone().handle_tool_call(
            crate::tools::router::ToolCall {
                tool_name: codex_tools::ToolName::plain("write_stdin"),
                call_id: "declared-validation-poll".into(),
                payload: ToolPayload::Function {
                    arguments: json!({"session_id": id, "chars": "", "yield_time_ms": 10000})
                        .to_string(),
                },
            },
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = settled
    else {
        panic!("normal stdin output")
    };
    let text = output.body.to_text().unwrap();
    assert!(text.contains("CHILD_FINISHED"), "{text}");
    assert!(text.contains("Process exited with code 0"), "{text}");
    assert!(
        text.contains("src/declared-only.rs") && text.contains("tests/declared-only.rs"),
        "{text}"
    );
    assert!(
        text.contains("unverified"),
        "successful execution must not claim proved coverage: {text}"
    );
    // Untagged commands preserve their existing result and do not inherit the prior process annotation.
    let plain = runtime.handle_tool_call(crate::tools::router::ToolCall {
        tool_name: codex_tools::ToolName::plain("exec_command"), call_id: "untagged-child".into(),
        payload: ToolPayload::Function { arguments: json!({"kind": "argv", "program": python, "args": ["-c", "print('PLAIN_CHILD')"], "yield_time_ms": 10000}).to_string() },
    }, tokio_util::sync::CancellationToken::new()).await.unwrap();
    let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = plain else {
        panic!("normal plain output")
    };
    let text = output.body.to_text().unwrap();
    assert!(
        text.contains("PLAIN_CHILD") && text.contains("Process exited with code 0"),
        "{text}"
    );
    assert!(
        !text.contains("declared-only") && !text.contains("coverage_status"),
        "{text}"
    );
    session
        .services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
}
