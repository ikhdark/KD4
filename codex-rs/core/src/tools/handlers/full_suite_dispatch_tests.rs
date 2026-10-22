//! Full-suite budget enforcement through codex-core's command handlers.

#[tokio::test]
async fn full_suite_owner_busy_releases_both_handler_reservations_without_launch() {
    use crate::session::step_context::StepContext;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::ExecCommandHandler;
    use crate::tools::handlers::ShellCommandHandler;
    use crate::tools::registry::ToolExecutor;
    use std::sync::Arc;
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(codex_features::Feature::Kd4Runtime)
        .unwrap();
    turn.approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)
        .unwrap();
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let temp = kd4_checkout();
    let root =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(temp.path().to_path_buf()).unwrap();
    let owner = crate::workspace_operation_gate::acquire_workspace_validation(
        &root,
        "cargo test -p holder",
        crate::workspace_operation_gate::ValidationOwner::for_command(
            codex_protocol::ThreadId::new(),
            &["cargo".into(), "test".into()],
        ),
    )
    .await
    .unwrap();
    let turn = Arc::new(turn);
    let session = Arc::new(session);
    let invocation = |tool_name| ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "busy-suite".into(),
        tool_name,
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({
                "program": temp.path().join("cargo.exe"), "args": ["test", "--no-fail-fast"],
                "workdir": temp.path(),
            })
            .to_string(),
        },
        cancellation_token: tokio_util::sync::CancellationToken::new(),
    };
    let exec = ExecCommandHandler::default();
    let shell = ShellCommandHandler::default();
    for result in [
        exec.handle_call(invocation(exec.tool_name())).await,
        shell.handle_call(invocation(shell.tool_name())).await,
    ] {
        assert!(
            result.is_err(),
            "the held owner must prevent any process launch"
        );
        let retry = turn.full_suite_budget.reserve(true, "later".into());
        assert!(
            retry.is_ok(),
            "zero-dispatch refusal must leave the allowance available"
        );
        drop(retry);
    }
    drop(owner);
}

fn kd4_checkout() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("codex-rs/.config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("kd4-rust-tests.toml"), "").unwrap();
    root
}

#[tokio::test]
async fn full_suite_both_handlers_enforce_shared_budget_even_with_force_fresh() {
    use crate::session::step_context::StepContext;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::ExecCommandHandler;
    use crate::tools::handlers::ShellCommandHandler;
    use crate::tools::registry::ToolExecutor;
    use std::sync::Arc;

    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(codex_features::Feature::Kd4Runtime)
        .unwrap();
    // A missing executable ensures even a broken guard cannot run a suite.
    let temp = kd4_checkout();
    let program = temp.path().join("cargo.exe");
    turn.full_suite_budget.admit(true).unwrap();
    let turn = Arc::new(turn);
    let session = Arc::new(session);
    let invocation = |tool_name| ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "full-suite-blocked".into(),
        tool_name,
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({
                "program": program, "args": ["test", "--no-fail-fast"], "force_fresh": true,
                "workdir": temp.path(),
            })
            .to_string(),
        },
        cancellation_token: tokio_util::sync::CancellationToken::new(),
    };
    let exec = ExecCommandHandler::default();
    let shell = ShellCommandHandler::default();
    for result in [
        exec.handle_call(invocation(exec.tool_name())).await,
        shell.handle_call(invocation(shell.tool_name())).await,
    ] {
        match result {
            Err(crate::FunctionCallError::RespondToModel(message)) => {
                assert!(message.contains("Full-suite launch blocked"), "{message}")
            }
            _ => panic!("both command handlers must reject a second full suite before launch"),
        }
    }
}
