//! Validation admission through both real command handlers, before child launch.

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::ExecCommandHandler;
use crate::tools::handlers::ShellCommandHandler;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::validation::ValidationMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

struct Fixture {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    root: tempfile::TempDir,
    cargo: PathBuf,
    just: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    async fn new(mode: ValidationMode) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("codex-rs/.config")).unwrap();
        std::fs::write(root.path().join("codex-rs/.config/kd4-rust-tests.toml"), "").unwrap();
        let marker = root.path().join("spawned.txt");
        let executable = |name: &str| {
            let path = root.path().join(if cfg!(windows) { format!("{name}.cmd") } else { name.into() });
            let source = if cfg!(windows) {
                format!("@echo off\r\n>> \"{}\" echo spawned\r\nexit /b 1\r\n", marker.display())
            } else {
                format!("#!/bin/sh\nprintf 'spawned\\n' >> '{}'\nexit 1\n", marker.display())
            };
            std::fs::write(&path, source).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        };
        let cargo = executable("cargo");
        let just = executable("just");
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.validation_mode = mode;
        turn.validation_full_budget = 2;
        turn.approval_policy.set(codex_protocol::protocol::AskForApproval::Never).unwrap();
        turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        let cwd = AbsolutePathBuf::try_from(root.path().to_path_buf()).unwrap();
        let config = Arc::make_mut(&mut turn.config);
        config.cwd = cwd.clone();
        config.features.enable(codex_features::Feature::Kd4Runtime).unwrap();
        let policy = &mut config.permissions.shell_environment_policy;
        policy.inherit = codex_protocol::config_types::ShellEnvironmentPolicyInherit::None;
        policy.r#set = std::env::vars().filter(|(key, _)| !key.to_ascii_uppercase().starts_with("CARGO_")).collect();
        turn.environments.turn_environments = vec![TurnEnvironment::new(
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            Arc::new(codex_exec_server::Environment::default_for_tests()),
            PathUri::from_abs_path(&cwd), None,
        )];
        Self { session: Arc::new(session), turn: Arc::new(turn), root, cargo, just, marker }
    }

    async fn call(&self, shell: bool, mut arguments: Value, nested: bool, id: &str)
        -> Result<Box<dyn ToolOutput>, crate::FunctionCallError>
    {
        arguments["workdir"] = json!(self.root.path());
        let invocation = ToolInvocation {
            session: Arc::clone(&self.session),
            step_context: StepContext::for_test(Arc::clone(&self.turn)),
            tracker: Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::new())),
            call_id: id.into(),
            tool_name: codex_tools::ToolName::plain(if shell { "shell_command" } else { "exec_command" }),
            source: if nested { ToolCallSource::CodeMode {
                cell_id: "validation-policy-cell".into(), parent_call_id: Some("parent".into()),
                runtime_tool_call_id: id.into(), nested_deadline: None, cancellation_cause: None,
            } } else { ToolCallSource::Direct },
            payload: ToolPayload::Function { arguments: arguments.to_string() },
            cancellation_token: tokio_util::sync::CancellationToken::new(),
        };
        if shell { ShellCommandHandler::default().handle(invocation).await }
        else { ExecCommandHandler::default().handle(invocation).await }
    }

    fn seed_started(&self, owner: &str, selection: &str, revision: &str) {
        let permit = self.turn.full_suite_budget.reserve_policy(
            owner.into(), selection.into(), revision.into(), 2,
            self.turn.validation_mode == ValidationMode::Compile,
        ).unwrap();
        self.turn.full_suite_budget.start(owner, None).unwrap();
        drop(permit);
    }

    fn assert_refusal(&self, output: Box<dyn ToolOutput>, mode: &str) -> String {
        let payload = ToolPayload::Function { arguments: "{}".into() };
        let ResponseInputItem::FunctionCallOutput { output: response, .. } =
            output.to_response_item("refused", &payload) else { panic!("function response") };
        assert_eq!(response.success, Some(false));
        let signal: Value = serde_json::from_str(&response.body.to_text().unwrap()).unwrap();
        assert_eq!(signal["validation_policy_refusal"], true);
        assert_eq!(signal["status"], "unvalidated");
        assert_eq!(signal["execution_state"], "not_started");
        assert_eq!(signal["mode"], mode);
        assert!(signal["reason"].as_str().is_some_and(|reason| !reason.is_empty()));
        assert_eq!(output.sampling_request_signal(), Some(signal.clone()));
        assert!(signal.get("validation_admission_pending").is_none());
        assert!(!self.marker.exists(), "refused command launched the marker process");
        signal["reason"].as_str().unwrap().into()
    }
}

#[tokio::test]
async fn off_refuses_validation_wrappers_for_direct_and_code_mode_calls() {
    let fixture = Fixture::new(ValidationMode::Off).await;
    let cargo = fixture.cargo.display().to_string();
    let ps = format!("& '{}' test -p codex-core", cargo.replace('\'', "''"));
    let bash = format!("'{}' test -p codex-core", cargo.replace('\\', "/"));
    for shell in [false, true] {
        let script_key = if shell { "command" } else { "cmd" };
        for nested in [false, true] {
            for (index, arguments) in [
                json!({"program": fixture.cargo, "args": ["test", "-p", "codex-core"]}),
                json!({"program": fixture.cargo, "args": ["check", "-p", "codex-core"]}),
                json!({"program": fixture.just, "args": ["core-test-fast", "core_lib", "-E", "test(=fixture)"]}),
                json!({"program": "bash", "args": ["-lc", bash]}),
                json!({"program": "pwsh", "args": ["-NoProfile", "-Command", ps]}),
                if cfg!(windows) { json!({"script_body": ps}) } else { json!({(script_key): bash}) },
            ].into_iter().enumerate() {
                let output = fixture.call(shell, arguments, nested, &format!("off-{shell}-{nested}-{index}")).await
                    .expect("policy refusal is successful tool transport");
                fixture.assert_refusal(output, "off");
            }
        }
    }
    assert_eq!(fixture.turn.full_suite_budget.policy_snapshot(2).admitted, 0);
}

#[tokio::test]
async fn compile_refuses_tests_and_admits_targeted_check() {
    for shell in [false, true] {
        let fixture = Fixture::new(ValidationMode::Compile).await;
        for (index, arguments) in [
            json!({"program": fixture.cargo, "args": ["test", "-p", "codex-core"]}),
            json!({"program": fixture.just, "args": ["test"]}),
        ].into_iter().enumerate() {
            let output = fixture.call(shell, arguments, false, &format!("compile-refused-{shell}-{index}")).await
                .expect("policy refusal transport");
            fixture.assert_refusal(output, "compile");
        }
        let output = fixture.call(shell,
            json!({"program": fixture.cargo, "args": ["check", "-p", "codex-core"]}),
            false, &format!("compile-admitted-{shell}"),
        ).await.expect("the fixture command executes");
        assert_eq!(output.outcome_for_logging(), codex_tools::ToolOutputOutcome::Failure);
        assert!(fixture.marker.exists());
        assert_eq!(fixture.turn.full_suite_budget.policy_snapshot(2).admitted, 1);
    }
}

#[tokio::test]
async fn full_refuses_a_third_attempt_after_two_started_validations() {
    let fixture = Fixture::new(ValidationMode::Full).await;
    fixture.seed_started("first", "focused-tests", "revision-one");
    fixture.seed_started("second", "gate", "revision-two");
    for shell in [false, true] {
        let output = fixture.call(shell,
            json!({"program": fixture.cargo, "args": ["test", "-p", "codex-core"]}),
            true, &format!("full-exhausted-{shell}"),
        ).await.expect("budget refusal transport");
        assert!(fixture.assert_refusal(output, "full").contains("budget exhausted"));
    }
    let snapshot = fixture.turn.full_suite_budget.policy_snapshot(2);
    assert_eq!(snapshot.admitted, 2);
    assert!(snapshot.exhausted);
}

#[tokio::test]
async fn compile_refuses_unchanged_inputs_even_for_a_different_selection() {
    let fixture = Fixture::new(ValidationMode::Compile).await;
    // This non-Git fixture has no source identity: the launch owner uses a stable sentinel.
    fixture.seed_started("previous", "a-different-target", "source-identity-unavailable");
    for shell in [false, true] {
        let output = fixture.call(shell,
            json!({"program": fixture.cargo, "args": ["check", "-p", "codex-core"], "force_fresh": true}),
            false, &format!("unchanged-{shell}"),
        ).await.expect("unchanged-input refusal transport");
        assert!(fixture.assert_refusal(output, "compile").contains("unchanged inputs"));
    }
    assert_eq!(fixture.turn.full_suite_budget.policy_snapshot(2).admitted, 1);
}

#[tokio::test]
async fn compile_and_full_refuse_multiple_validation_leaves_in_one_shell_launch() {
    for (mode, wire) in [(ValidationMode::Compile, "compile"), (ValidationMode::Full, "full")] {
        let fixture = Fixture::new(mode).await;
        let cargo = fixture.cargo.display().to_string();
        for shell in [false, true] {
            let arguments = if cfg!(windows) {
                let cargo = cargo.replace('\'', "''");
                json!({"script_body": format!("& '{cargo}' check -p codex-core; & '{cargo}' check -p codex-utils")})
            } else {
                let key = if shell { "command" } else { "cmd" };
                json!({(key): format!("'{cargo}' check -p codex-core; '{cargo}' check -p codex-utils")})
            };
            let output = fixture.call(shell, arguments, true, &format!("multiple-leaves-{wire}-{shell}")).await
                .expect("multi-validation refusal transport");
            assert!(fixture.assert_refusal(output, wire).contains("Multiple validation launches"));
        }
        assert_eq!(fixture.turn.full_suite_budget.policy_snapshot(2).admitted, 0);
    }
}
