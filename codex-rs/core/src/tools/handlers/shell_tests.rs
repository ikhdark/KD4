use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_exec_server::Environment;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::NetworkPermissions;
use codex_protocol::models::ShellCommandToolCallParams;
use codex_protocol::plan_tool::ValidationRouteLeaf;
use codex_protocol::validation::ValidationCommandContext;
use pretty_assertions::assert_eq;

use crate::config::PermissionProfileSnapshot;
use crate::exec::ExecExpiration;
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::sandboxing::SandboxPermissions;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::ShellCommandHandler;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::shell::shell_command::resolve_command_shell;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::turn_diff_tracker::TurnDiffTracker;
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

fn structured_cargo_leaf() -> ValidationRouteLeaf {
    ValidationRouteLeaf {
        argv: vec![
            "cargo".into(),
            "test".into(),
            "-p".into(),
            "codex-core".into(),
            "focused_case".into(),
            "--".into(),
            "--exact".into(),
        ],
        uncertainty: "the focused case still satisfies its contract".into(),
        covered_paths: vec!["core/src/task_evidence.rs".into()],
        covered_contracts: vec!["focused-validation-v1".into()],
        timeout_ms: 30_000,
        semantic_timeout: false,
    }
}

#[test]
fn command_runner_structured_validation_distinguishes_feature_scope_from_target_scope() {
    let repo = std::path::Path::new(".");
    let mut leaf = structured_cargo_leaf();
    leaf.argv.insert(4, "--all-features".into());
    assert!(super::validate_structured_validation_leaf(&leaf, repo).is_ok());

    leaf.argv[4] = "--all-targets".into();
    assert!(
        super::validate_structured_validation_leaf(&leaf, repo)
            .expect_err("all targets is a broad validation route")
            .contains("must remain focused")
    );
}

#[test]
fn command_runner_admission_routes_are_consistent() {
    let repo = std::path::Path::new(".");

    for argv in [
        vec![
            "just".into(),
            "test-fast".into(),
            "-p".into(),
            "codex-core".into(),
            "--all-targets".into(),
            "-E".into(),
            "test(=focused_case)".into(),
        ],
        vec![
            "just".into(),
            "check-lane".into(),
            "codex-core".into(),
            "--workspace".into(),
        ],
    ] {
        let mut leaf = structured_cargo_leaf();
        leaf.argv = argv;
        assert!(
            super::validate_structured_validation_leaf(&leaf, repo).is_err(),
            "broad Just route must be rejected: {:?}",
            leaf.argv
        );
    }

    for argv in [
        vec![
            "cargo".into(),
            "test".into(),
            "focused_case".into(),
            "--".into(),
            "-p".into(),
            "codex-core".into(),
            "--exact".into(),
        ],
        vec![
            "cargo".into(),
            "check".into(),
            "--".into(),
            "--package".into(),
            "codex-core".into(),
        ],
    ] {
        let mut leaf = structured_cargo_leaf();
        leaf.argv = argv;
        assert!(
            super::validate_structured_validation_leaf(&leaf, repo).is_err(),
            "Cargo package scope after `--` must not satisfy admission: {:?}",
            leaf.argv
        );
    }

    let pytest_repo = tempfile::tempdir().expect("pytest repository tempdir");
    std::fs::create_dir(pytest_repo.path().join("tests"))
        .expect("pytest test directory is created");
    for selector in [".", "tests"] {
        let mut leaf = structured_cargo_leaf();
        leaf.argv = vec![
            "python".into(),
            "-m".into(),
            "pytest".into(),
            selector.into(),
        ];
        assert!(
            super::validate_structured_validation_leaf(&leaf, pytest_repo.path())
                .expect_err("directory-wide pytest selector must be rejected")
                .contains("not a directory")
        );
    }
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
fn recommended_fixes_structured_validation_rejects_zero_test_selectors() {
    let repo = std::path::Path::new(".");
    let valid = structured_cargo_leaf();
    assert!(super::validate_structured_validation_leaf(&valid, repo).is_ok());

    let mut missing_uncertainty = valid.clone();
    missing_uncertainty.uncertainty.clear();
    assert!(
        super::validate_structured_validation_leaf(&missing_uncertainty, repo)
            .expect_err("missing uncertainty")
            .contains("uncertainty")
    );

    let mut missing_coverage = valid.clone();
    missing_coverage.covered_paths.clear();
    assert!(
        super::validate_structured_validation_leaf(&missing_coverage, repo)
            .expect_err("missing coverage")
            .contains("covered_paths")
    );

    let mut broad = valid;
    broad.argv = vec!["cargo".into(), "test".into()];
    assert!(
        super::validate_structured_validation_leaf(&broad, repo)
            .expect_err("broad cargo test")
            .contains("one exact test ID")
    );

    broad.argv = vec![
        "cargo".into(),
        "test".into(),
        "-p".into(),
        "codex-core".into(),
        "--test".into(),
        "integration".into(),
    ];
    assert!(
        super::validate_structured_validation_leaf(&broad, repo)
            .expect_err("cargo target is not a libtest filter")
            .contains("one exact test ID")
    );

    let mut unsafe_coverage = structured_cargo_leaf();
    unsafe_coverage.covered_paths = vec!["../outside.rs".into()];
    assert!(
        super::validate_structured_validation_leaf(&unsafe_coverage, repo)
            .expect_err("coverage traversal")
            .contains("must stay within the repository")
    );

    let mut broad_check = structured_cargo_leaf();
    broad_check.argv = vec!["cargo".into(), "check".into()];
    assert!(
        super::validate_structured_validation_leaf(&broad_check, repo)
            .expect_err("workspace-wide cargo check")
            .contains("must name a package")
    );

    let mut selectorless_pytest = structured_cargo_leaf();
    selectorless_pytest.argv = vec!["python".into(), "-m".into(), "pytest".into(), "-q".into()];
    assert!(
        super::validate_structured_validation_leaf(&selectorless_pytest, repo)
            .expect_err("selectorless pytest")
            .contains("focused test selector")
    );

    let mut broad_package_lane = structured_cargo_leaf();
    broad_package_lane.argv = vec![
        "just".into(),
        "test-lane-package".into(),
        "codex-core".into(),
    ];
    assert!(
        super::validate_structured_validation_leaf(&broad_package_lane, repo)
            .expect_err("package-wide nextest")
            .contains("must name a test filter")
    );

    let mut raw_nextest_selector = structured_cargo_leaf();
    raw_nextest_selector.argv = vec![
        "just".into(),
        "test-fast".into(),
        "-p".into(),
        "codex-core".into(),
        "module::case".into(),
    ];
    assert!(
        super::validate_structured_validation_leaf(&raw_nextest_selector, repo)
            .expect_err("raw nextest selector must be rejected before launch")
            .contains("exact `-E test(=...)` selector")
    );
}

#[test]
fn direct_validation_requires_explicit_uncertainty_and_direct_argv() {
    let context = ValidationCommandContext {
        uncertainty: "the focused shell contract remains satisfied".to_string(),
        covered_paths: vec!["core/src/task_evidence.rs".to_string()],
        covered_contracts: vec!["direct-validation-v1".to_string()],
    };
    let invocation = CommandInvocation::Argv {
        program: "cargo".to_string(),
        args: vec![
            "test".to_string(),
            "-p".to_string(),
            "codex-core".to_string(),
            "direct_validation".to_string(),
            "--".to_string(),
            "--exact".to_string(),
        ],
    };
    let route =
        super::direct_validation_route(&context, &invocation, std::path::Path::new("."), 45_000)
            .expect("focused direct validation route");
    assert_eq!(route.leaves.len(), 1);
    assert_eq!(route.leaves[0].uncertainty, context.uncertainty);
    assert_eq!(route.leaves[0].covered_paths, context.covered_paths);

    let script =
        CommandInvocation::Script("cargo test -p codex-core direct_validation".to_string());
    assert!(
        super::direct_validation_route(&context, &script, std::path::Path::new("."), 45_000)
            .expect_err("shell validation must not infer coverage from a script")
            .contains("direct argv")
    );
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
        "zsh fork setup failed".to_string(),
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
fn validation_execution_outcome_distinguishes_success_from_not_executed() {
    let skipped = super::RunExecLikeResult {
        output: FunctionToolOutput::from_text("validation skipped".to_string(), Some(true)),
        exit_code: None,
        validation_execution_outcome: super::ValidationExecutionOutcome::ExecutedSuccess,
        canonical_output: None,
    };
    assert_eq!(
        skipped.validation_execution_outcome(),
        super::ValidationExecutionOutcome::ExecutedSuccess
    );

    let unknown_exit = super::RunExecLikeResult {
        output: FunctionToolOutput::from_text("no exit".to_string(), Some(true)),
        exit_code: None,
        validation_execution_outcome: super::ValidationExecutionOutcome::NotExecuted,
        canonical_output: None,
    };
    assert_eq!(
        unknown_exit.validation_execution_outcome(),
        super::ValidationExecutionOutcome::NotExecuted
    );

    let projected = super::shell_command::validation_structured_output(serde_json::json!({
        "text": "validation skipped",
        "execution_outcome": "not_executed",
        "command_was_executed": false,
        "skip_disposition": "deferred",
    }));
    assert_eq!(
        projected.outcome_context(),
        codex_tools::ToolOutputOutcomeContext::skipped(Some(
            codex_tools::ToolOutputSkipDisposition::Deferred,
        ))
    );
    assert!(!projected.success_for_logging());

    let not_applicable = super::shell_command::validation_structured_output(serde_json::json!({
        "text": "running 0 tests",
        "execution_outcome": "executed_not_applicable",
        "command_was_executed": true,
        "skip_disposition": "not_applicable",
    }));
    assert_eq!(
        not_applicable.outcome_context(),
        codex_tools::ToolOutputOutcomeContext::skipped(Some(
            codex_tools::ToolOutputSkipDisposition::NotApplicable,
        ))
    );
    assert!(!not_applicable.success_for_logging());
}

#[test]
fn legacy_shell_projection_metadata_keeps_exact_bytes_status_and_context() {
    let raw = vec![b'o', b'k', b'\n', 0xff];
    let output = super::LegacyShellToolOutput {
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

#[tokio::test]
async fn validation_owner_retains_sole_waiter_until_worker_completion() {
    let registry = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let invocation = CommandInvocation::Script("cargo test -p codex-core".to_string());
    let identity = crate::validation_admission::validation_identity(
        b"repo",
        "codex-rs",
        &invocation,
        "env",
        "stable",
        1,
    );
    let caller_cancellation = tokio_util::sync::CancellationToken::new();
    let registration = crate::validation_admission::register_if_absent(
        &registry,
        identity,
        "leader-call",
        &caller_cancellation,
    )
    .await;
    let roles = super::shell_command::validation_registration_roles(registration);
    assert!(roles.worker_waiter.is_none());
    let execution = roles.execution.expect("leader execution ownership");
    let owner_waiter = roles.owner_waiter.expect("leader owner waiter");
    let execution_cancellation = execution.cancellation_token();
    let worker_cancellation = execution_cancellation.clone();
    let worker = tokio::spawn(async move {
        tokio::task::yield_now().await;
        assert!(!worker_cancellation.is_cancelled());
        execution
            .complete(crate::validation_admission::ReusableValidationResult {
                value: serde_json::json!({"success": true}),
            })
            .await;
    });

    super::shell_command::await_validation_execution(worker, Some(owner_waiter))
        .await
        .expect("validation worker should complete");
}

#[test]
fn known_delta_reuses_only_complete_successes() {
    let mut output = codex_protocol::exec_output::ExecToolCallOutput::default();
    assert!(super::is_complete_success(&output));

    output.exit_code = 1;
    assert!(!super::is_complete_success(&output));
    output.exit_code = 0;
    output.timed_out = true;
    assert!(!super::is_complete_success(&output));
    output.timed_out = false;
    output.aggregated_output.truncated_after_lines = Some(1);
    assert!(!super::is_complete_success(&output));
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

fn validation_argv(program: &str, args: &[&str]) -> Vec<String> {
    std::iter::once(program.to_string())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect()
}

fn validation_summary(command: &[String]) -> String {
    CommandInvocation::Argv {
        program: command[0].clone(),
        args: command[1..].to_vec(),
    }
    .display_command()
}

fn admit_validation(command: Vec<String>) -> Result<String, String> {
    let repo_root = std::env::current_dir().expect("test process has a current directory");
    admit_validation_in_repo(command, &repo_root)
}

fn admit_validation_in_repo(
    command: Vec<String>,
    repo_root: &std::path::Path,
) -> Result<String, String> {
    let summary = validation_summary(&command);
    super::focused_validation_command_summary(
        &command,
        &summary,
        /*direct_argv*/ true,
        repo_root,
        repo_root,
        &ExecExpiration::Timeout(Duration::from_secs(60)),
        /*sandbox_override*/ false,
        /*additional_permissions*/ false,
        /*prefix_rule*/ false,
    )
}

#[cfg(unix)]
fn create_directory_link(target: &std::path::Path, link: &std::path::Path) {
    std::os::unix::fs::symlink(target, link).expect("directory symlink is created");
}

#[cfg(windows)]
fn create_directory_link(target: &std::path::Path, link: &std::path::Path) {
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()
        .expect("junction command starts");
    assert!(status.success(), "directory junction is created");
}

#[cfg(unix)]
fn remove_directory_link(link: &std::path::Path) {
    std::fs::remove_file(link).expect("directory symlink is removed");
}

#[cfg(windows)]
fn remove_directory_link(link: &std::path::Path) {
    std::fs::remove_dir(link).expect("directory junction is removed");
}

#[test]
fn focused_validation_accepts_closed_cargo_just_and_python_forms() {
    for command in [
        validation_argv("cargo", &["check", "-p", "codex-core"]),
        validation_argv(
            "cargo",
            &["test", "-p", "codex-core", "--lib", "--", "--nocapture"],
        ),
        validation_argv("just", &["source-map-check"]),
        validation_argv("just", &["fmt-check"]),
        validation_argv(
            "just",
            &[
                "test-fast",
                "-p",
                "codex-core",
                "-E",
                "test(=focused_validation)",
            ],
        ),
        validation_argv(
            "just",
            &[
                "test-compile",
                "-p",
                "codex-core",
                "-E",
                "test(=focused_validation)",
            ],
        ),
        validation_argv(
            "just",
            &[
                "test-lane",
                "typed-validation",
                "-p",
                "codex-core",
                "-E",
                "test(=focused_validation)",
            ],
        ),
        validation_argv(
            "just",
            &[
                "test-lane-fast",
                "typed-validation",
                "-p",
                "codex-core",
                "-E",
                "test(=focused_validation)",
            ],
        ),
        validation_argv(
            "just",
            &[
                "test-lane-main",
                "-p",
                "codex-core",
                "-E",
                "test(=focused_validation)",
            ],
        ),
        validation_argv(
            "just",
            &[
                "test-lane-package",
                "codex-core",
                "-E",
                "test(=focused_validation)",
            ],
        ),
        validation_argv(
            "just",
            &["check-lane", "codex-core", "--features", "test-support"],
        ),
        validation_argv("python", &["-m", "unittest", "scripts.test_policy"]),
        validation_argv(
            "python",
            &[
                "-m",
                "unittest",
                "discover",
                "--start-directory=scripts",
                "--pattern",
                "test_*.py",
            ],
        ),
        validation_argv(
            "python3",
            &[
                "-m",
                "pytest",
                "-q",
                "--ignore=scripts/generated",
                "scripts/tests/test_policy.py::test_closed",
            ],
        ),
    ] {
        assert_eq!(
            admit_validation(command.clone()),
            Ok(validation_summary(&command))
        );
    }
}

#[test]
fn focused_validation_rejects_mutating_or_redirected_cargo_and_just_forms() {
    for command in [
        validation_argv("cargo", &["run"]),
        validation_argv("cargo", &["install", "cargo-nextest"]),
        validation_argv("cargo", &["fix"]),
        validation_argv("cargo", &["--locked", "test"]),
        validation_argv("cargo", &["test", "--manifest-path", "../other/Cargo.toml"]),
        validation_argv("cargo", &["test", "--config=net.git-fetch-with-cli=true"]),
        validation_argv("cargo", &["check", "--target-dir", "../target"]),
        validation_argv("cargo", &["test", "-Zunstable-options"]),
        validation_argv("cargo", &["test", ">", "results.txt"]),
        validation_argv("just", &["fmt"]),
        validation_argv("just", &["fix"]),
        validation_argv("just", &["test-force"]),
        validation_argv("just", &["publish"]),
        validation_argv("just", &["cleanup"]),
        validation_argv("just", &["write-config-schema"]),
        validation_argv("just", &["test-fast", "--justfile", "../Justfile"]),
        validation_argv("just", &["source-map-check", "publish"]),
        validation_argv("just", &["fmt-check", "test-fast"]),
        validation_argv("just", &["test-lane"]),
        validation_argv("just", &["test-lane", "..", "-p", "codex-core"]),
        validation_argv("just", &["test-lane", "../shared", "-p", "codex-core"]),
        validation_argv(
            "just",
            &["test-lane-fast", "C:\\outside", "-p", "codex-core"],
        ),
        validation_argv("just", &["test-lane-package", "/tmp/output"]),
        validation_argv("just", &["test-lane-package", "~cache"]),
        validation_argv("just", &["check-lane", "-p"]),
    ] {
        assert!(
            admit_validation(command.clone()).is_err(),
            "unexpected admission: {}",
            validation_summary(&command)
        );
    }
}

#[test]
fn focused_validation_rejects_nextest_debug_archive_config_and_output_controls() {
    for forwarded in [
        vec!["--debugger", "lldb"],
        vec!["--tracer", "strace"],
        vec!["archive"],
        vec!["--archive-file", "result.tar.zst"],
        vec!["--extract-to", "target/extracted"],
        vec!["--persist-extract-tempdir"],
        vec!["--remap-path-prefix", "old=new"],
        vec!["--workspace-remap", "workspace"],
        vec!["--target-dir-remap", "target"],
        vec!["--cargo-metadata", "metadata.json"],
        vec!["--binaries-metadata", "binaries.json"],
        vec!["--config-file", "nextest.toml"],
        vec!["--tool-config-file", "tool.toml"],
        vec!["--user-config-file", "user.toml"],
        vec!["--profile", "local"],
        vec!["--cargo-profile", "dev"],
        vec!["--metadata"],
        vec!["--rerun", "previous"],
        vec!["--output", "json"],
        vec!["--output-dir", "reports"],
        vec!["--success-output", "final"],
        vec!["--message-format", "libtest-json"],
        vec!["--no-tests", "pass"],
        vec!["--flaky-result", "pass"],
        vec!["--pass-on-success"],
    ] {
        let mut args = vec!["test-fast"];
        args.extend(forwarded);
        let command = validation_argv("just", &args);
        assert!(
            admit_validation(command.clone()).is_err(),
            "unexpected admission: {}",
            validation_summary(&command)
        );
    }
}

#[test]
fn focused_validation_rejects_python_code_config_plugins_and_outside_paths() {
    for command in [
        validation_argv("python", &["-c", "print('no')"]),
        validation_argv("python", &["scripts/test_policy.py"]),
        validation_argv("python", &["-m", "compileall", "."]),
        validation_argv("python", &["-m", "pytest", "-c", "../pytest.ini"]),
        validation_argv("python", &["-m", "pytest", "-p", "arbitrary_plugin"]),
        validation_argv("python", &["-m", "pytest", "--trace"]),
        validation_argv("python", &["-m", "pytest", "--pdb"]),
        validation_argv(
            "python",
            &["-m", "pytest", "--pdbcls=debugpy._vendored.pydevd:PyDB"],
        ),
        validation_argv("python", &["-m", "pytest", "--rootdir=..", "tests"]),
        validation_argv("python", &["-m", "pytest", "--ignore=../../x"]),
        validation_argv(
            "python",
            &["-m", "pytest", "--ignore", "C:\\outside\\tests"],
        ),
        validation_argv("python", &["-m", "pytest", "--junitxml=C:\\outside.xml"]),
        validation_argv(
            "python",
            &["-m", "pytest", "--junitxml", "reports/results.xml"],
        ),
        validation_argv("python", &["-m", "pytest", "../outside/test_policy.py"]),
        validation_argv("python", &["-m", "pytest", "@pytest-args.txt"]),
        validation_argv(
            "python",
            &["-m", "unittest", "discover", "--start-directory=.."],
        ),
        validation_argv("python", &["-m", "unittest", "--start-directory=.."]),
        validation_argv(
            "python",
            &["-m", "unittest", "discover", "-s", "C:\\outside\\tests"],
        ),
        validation_argv("python", &["-m", "unittest", "..\\outside\\test_policy.py"]),
    ] {
        assert!(
            admit_validation(command.clone()).is_err(),
            "unexpected admission: {}",
            validation_summary(&command)
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn focused_validation_resolves_python_paths_through_links_and_existing_ancestors() {
    let repository = tempfile::tempdir().expect("repository tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let inside_tests = repository.path().join("tests");
    std::fs::create_dir(&inside_tests).expect("inside test directory is created");
    std::fs::write(
        inside_tests.join("test_inside.py"),
        b"def test_inside(): pass\n",
    )
    .expect("inside test file is created");
    std::fs::write(
        outside.path().join("test_external.py"),
        b"def test_external(): pass\n",
    )
    .expect("outside test file is created");
    let linked_tests = repository.path().join("linked_tests");
    create_directory_link(outside.path(), &linked_tests);

    let escaped_pytest = admit_validation_in_repo(
        validation_argv("python", &["-m", "pytest", "linked_tests/test_external.py"]),
        repository.path(),
    );
    let escaped_discovery = admit_validation_in_repo(
        validation_argv(
            "python",
            &["-m", "unittest", "discover", "-s", "linked_tests"],
        ),
        repository.path(),
    );
    let escaped_module = admit_validation_in_repo(
        validation_argv("python", &["-m", "unittest", "linked_tests.test_external"]),
        repository.path(),
    );
    let inside_existing = admit_validation_in_repo(
        validation_argv("python", &["-m", "pytest", "tests/test_inside.py"]),
        repository.path(),
    );
    let inside_nonexistent = admit_validation_in_repo(
        validation_argv("python", &["-m", "pytest", "tests/future/test_new.py"]),
        repository.path(),
    );
    let inside_module = admit_validation_in_repo(
        validation_argv("python", &["-m", "unittest", "tests.test_inside"]),
        repository.path(),
    );
    remove_directory_link(&linked_tests);

    assert!(escaped_pytest.is_err());
    assert!(escaped_discovery.is_err());
    assert!(escaped_module.is_err());
    assert!(inside_existing.is_ok());
    assert!(inside_nonexistent.is_ok());
    assert!(inside_module.is_ok());
}

#[test]
fn focused_validation_pins_trusted_executable_and_rejects_path_shadowing() {
    let repository = tempfile::tempdir().expect("repository tempdir");
    let fake_name = if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    };
    std::fs::copy(
        std::env::current_exe().expect("current test executable"),
        repository.path().join(fake_name),
    )
    .expect("fake executable is copied into repository");

    let nominal = validation_argv("python", &["-m", "pytest"]);
    let relative_path = std::env::join_paths([std::path::Path::new(".")])
        .expect("relative PATH joins")
        .to_string_lossy()
        .into_owned();
    let relative_env = std::collections::HashMap::from([("PATH".to_string(), relative_path)]);
    let mut relative_execution = nominal.clone();
    assert!(
        super::pin_focused_validation_executable(
            &mut relative_execution,
            &nominal,
            &relative_env,
            repository.path(),
            repository.path(),
        )
        .is_err()
    );

    let repo_path = std::env::join_paths([repository.path()])
        .expect("repository PATH joins")
        .to_string_lossy()
        .into_owned();
    let repo_env = std::collections::HashMap::from([("PATH".to_string(), repo_path)]);
    let mut repo_execution = nominal.clone();
    assert!(
        super::pin_focused_validation_executable(
            &mut repo_execution,
            &nominal,
            &repo_env,
            repository.path(),
            repository.path(),
        )
        .is_err()
    );

    let current_executable =
        std::fs::canonicalize(std::env::current_exe().expect("current test executable"))
            .expect("current test executable canonicalizes");
    let trusted_program = current_executable
        .file_name()
        .expect("current test executable has a filename")
        .to_string_lossy()
        .into_owned();
    let trusted_parent = current_executable
        .parent()
        .expect("current test executable has a parent");
    let trusted_path = std::env::join_paths([trusted_parent])
        .expect("trusted PATH joins")
        .to_string_lossy()
        .into_owned();
    let trusted_env = std::collections::HashMap::from([("PATH".to_string(), trusted_path)]);
    let trusted_nominal = vec![trusted_program];
    let mut trusted_execution = trusted_nominal.clone();
    let resolved = super::pin_focused_validation_executable(
        &mut trusted_execution,
        &trusted_nominal,
        &trusted_env,
        repository.path(),
        repository.path(),
    )
    .expect("trusted executable resolves and pins");
    assert_eq!(resolved, current_executable.to_string_lossy());
    assert_eq!(trusted_execution[0], resolved);
}

#[test]
fn focused_validation_rejects_sticky_pregranted_permissions() {
    let pregranted = super::super::EffectiveAdditionalPermissions {
        sandbox_permissions: SandboxPermissions::WithAdditionalPermissions,
        additional_permissions: Some(AdditionalPermissionProfile {
            network: Some(NetworkPermissions {
                enabled: Some(true),
            }),
            file_system: None,
        }),
        permissions_preapproved: true,
    };
    assert!(
        super::reject_focused_effective_permissions(/*focused_validation*/ true, &pregranted,)
            .is_err()
    );
    assert!(
        super::reject_focused_effective_permissions(/*focused_validation*/ false, &pregranted,)
            .is_ok()
    );
}

#[test]
fn focused_validation_rejects_wrappers_chaining_and_noncanonical_summary() {
    for command in [
        validation_argv("cmd", &["/c", "cargo", "test"]),
        validation_argv("pwsh", &["-Command", "cargo test"]),
        validation_argv("cargo", &["test", "&&", "git", "status"]),
        validation_argv("cargo", &["test", "2>", "results.txt"]),
    ] {
        assert!(admit_validation(command).is_err());
    }

    let command = validation_argv("cargo", &["test"]);
    assert!(
        super::focused_validation_command_summary(
            &command,
            "cargo  test",
            /*direct_argv*/ true,
            std::path::Path::new("repo"),
            std::path::Path::new("repo"),
            &ExecExpiration::Timeout(Duration::from_secs(60)),
            /*sandbox_override*/ false,
            /*additional_permissions*/ false,
            /*prefix_rule*/ false,
        )
        .is_err()
    );
}

#[test]
fn focused_validation_requires_repo_root_explicit_timeout_and_default_permissions() {
    let command = validation_argv("cargo", &["test"]);
    let summary = validation_summary(&command);
    let check = |direct_argv,
                 cwd: &std::path::Path,
                 expiration: &ExecExpiration,
                 sandbox_override,
                 additional_permissions,
                 prefix_rule| {
        super::focused_validation_command_summary(
            &command,
            &summary,
            direct_argv,
            cwd,
            std::path::Path::new("repo"),
            expiration,
            sandbox_override,
            additional_permissions,
            prefix_rule,
        )
    };

    for (direct_argv, cwd, expiration, sandbox_override, additional_permissions, prefix_rule) in [
        (
            false,
            "repo",
            ExecExpiration::Timeout(Duration::from_secs(60)),
            false,
            false,
            false,
        ),
        (
            true,
            "repo/subdir",
            ExecExpiration::Timeout(Duration::from_secs(60)),
            false,
            false,
            false,
        ),
        (
            true,
            "repo",
            ExecExpiration::DefaultTimeout,
            false,
            false,
            false,
        ),
        (
            true,
            "repo",
            ExecExpiration::Timeout(Duration::from_millis(
                super::MAX_FOCUSED_VALIDATION_TIMEOUT_MS + 1,
            )),
            false,
            false,
            false,
        ),
        (
            true,
            "repo",
            ExecExpiration::Timeout(Duration::from_secs(60)),
            true,
            false,
            false,
        ),
        (
            true,
            "repo",
            ExecExpiration::Timeout(Duration::from_secs(60)),
            false,
            true,
            false,
        ),
        (
            true,
            "repo",
            ExecExpiration::Timeout(Duration::from_secs(60)),
            false,
            false,
            true,
        ),
    ] {
        assert!(
            check(
                direct_argv,
                std::path::Path::new(cwd),
                &expiration,
                sandbox_override,
                additional_permissions,
                prefix_rule,
            )
            .is_err()
        );
    }
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

#[cfg(windows)]
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
            .to_safety_args(&shell, use_login_shell);

    assert!(safety_command.iter().any(|arg| arg == "-NoProfile"));
    assert!(is_known_safe_command(&safety_command));
}

/// The logic for is_known_safe_command() has heuristics for known shells,
/// so we must ensure the commands generated by [ShellCommandHandler] can be
/// recognized as safe if the `command` is safe.
#[test]
fn commands_generated_by_shell_command_handler_can_be_matched_by_is_known_safe_command() {
    let bash_shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };
    assert_safe(&bash_shell, "ls -la");

    let zsh_shell = Shell {
        shell_type: ShellType::Zsh,
        shell_path: PathBuf::from("/bin/zsh"),
    };
    assert_safe(&zsh_shell, "ls -la");

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
    let login_command_is_safe =
        is_known_safe_command(&shell.derive_exec_args(command, /*use_login_shell*/ true));
    if shell.shell_type == ShellType::PowerShell {
        assert!(!login_command_is_safe);
    } else {
        assert!(login_command_is_safe);
    }
    assert!(is_known_safe_command(
        &shell.derive_exec_args(command, /*use_login_shell*/ false)
    ));
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
            shell_type: ShellType::Bash,
            shell_path: PathBuf::from("/bin/bash"),
        }),
    );
    let invocation = CommandInvocation::PowerShellScript("Get-ChildItem".to_string());
    let session_shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
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
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/selected/bin/bash"),
    };
    let expected_command = selected_shell.derive_exec_args(&command, /*use_login_shell*/ true);
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
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let login_command = ShellCommandHandler::base_command(
        &shell,
        "echo login shell",
        /*use_login_shell*/ true,
    );
    assert_eq!(
        login_command,
        shell.derive_exec_args("echo login shell", /*use_login_shell*/ true)
    );

    let non_login_command = ShellCommandHandler::base_command(
        &shell,
        "echo non login shell",
        /*use_login_shell*/ false,
    );
    assert_eq!(
        non_login_command,
        shell.derive_exec_args("echo non login shell", /*use_login_shell*/ false)
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
fn focused_validation_aggregates_independent_capability_denials() {
    let command = validation_argv("bash", &["&&"]);
    let denial = super::focused_validation_command_summary(
        &command,
        "not canonical",
        /*direct_argv*/ false,
        std::path::Path::new("repo/subdir"),
        std::path::Path::new("repo"),
        &ExecExpiration::DefaultTimeout,
        /*sandbox_override*/ true,
        /*additional_permissions*/ true,
        /*prefix_rule*/ true,
    )
    .expect_err("invalid envelope and argv should be denied together");
    let encoded = denial
        .strip_prefix("FocusedValidationCapabilityDenied: ")
        .expect("structured denial prefix");
    let denial: serde_json::Value = serde_json::from_str(encoded).expect("structured denial JSON");
    let codes = denial["violations"]
        .as_array()
        .expect("violation array")
        .iter()
        .map(|violation| violation["code"].as_str().expect("violation code"))
        .collect::<Vec<_>>();
    assert_eq!(
        codes,
        vec![
            "direct_argv_required",
            "repository_root_cwd_required",
            "explicit_timeout_required",
            "default_permissions_required",
            "shell_control_argument_forbidden",
            "validation_program_required",
            "canonical_summary_required",
        ]
    );
    assert!(denial.get("canonical_permitted_command").is_none());
}

#[test]
fn focused_validation_only_suggests_a_canonical_command_for_permitted_argv() {
    let command = validation_argv("cargo", &["test", "-p", "codex-core"]);
    let summary = validation_summary(&command);
    let denial = super::focused_validation_command_summary(
        &command,
        &summary,
        /*direct_argv*/ true,
        std::path::Path::new("repo/subdir"),
        std::path::Path::new("repo"),
        &ExecExpiration::Timeout(Duration::from_secs(60)),
        /*sandbox_override*/ false,
        /*additional_permissions*/ false,
        /*prefix_rule*/ false,
    )
    .expect_err("wrong cwd should still deny an otherwise valid command");
    let encoded = denial
        .strip_prefix("FocusedValidationCapabilityDenied: ")
        .expect("structured denial prefix");
    let denial: serde_json::Value = serde_json::from_str(encoded).expect("structured denial JSON");
    assert_eq!(denial["canonical_permitted_command"], summary);
}

#[test]
fn focused_validation_invalid_program_does_not_cascade_shape_violations() {
    let command = validation_argv("bash", &["test", "--workspace"]);
    let summary = validation_summary(&command);
    let denial = super::focused_validation_command_summary(
        &command,
        &summary,
        /*direct_argv*/ true,
        std::path::Path::new("repo"),
        std::path::Path::new("repo"),
        &ExecExpiration::Timeout(Duration::from_secs(60)),
        /*sandbox_override*/ false,
        /*additional_permissions*/ false,
        /*prefix_rule*/ false,
    )
    .expect_err("unsupported executable should be denied");
    let encoded = denial
        .strip_prefix("FocusedValidationCapabilityDenied: ")
        .expect("structured denial prefix");
    let denial: serde_json::Value = serde_json::from_str(encoded).expect("structured denial JSON");
    let codes = denial["violations"]
        .as_array()
        .expect("violation array")
        .iter()
        .map(|violation| violation["code"].as_str().expect("violation code"))
        .collect::<Vec<_>>();
    assert_eq!(codes, vec!["validation_program_required"]);
    assert!(denial.get("canonical_permitted_command").is_none());
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
    let handler = ShellCommandHandler::from(codex_tools::ShellCommandBackendConfig::Classic);

    assert_eq!(
        handler.pre_tool_use_payload(&ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
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
    let handler = ShellCommandHandler::from(codex_tools::ShellCommandBackendConfig::Classic);
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
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
    let exec_args = command.to_exec_args(&powershell, /*use_login_shell*/ false);

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
    let handler = ShellCommandHandler::from(codex_tools::ShellCommandBackendConfig::Classic);
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
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
    let handler = ShellCommandHandler::from(codex_tools::ShellCommandBackendConfig::Classic);
    let output = handler
        .handle(ToolInvocation {
            session: session.into(),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
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
    let handler = ShellCommandHandler::from(codex_tools::ShellCommandBackendConfig::Classic);
    let invoke = |call_id: &str| ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn: Arc::clone(&turn),
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
        success: Some(true),
        outcome: None,
        post_tool_use_response: Some(json!("shell output")),
        deterministic_continuation_receipts: Vec::new(),
        sampling_request_signal: None,
        deterministic_continuation_owner_key: None,
        skip_disposition: None,
    };
    let handler = ShellCommandHandler::from(codex_tools::ShellCommandBackendConfig::Classic);
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
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
}
