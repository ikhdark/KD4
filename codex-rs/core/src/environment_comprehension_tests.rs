//! Cross-layer regressions: model context must describe dispatch's selected state.
use super::*;
use crate::context::ContextualUserFragment;
use crate::context::world_state::EnvironmentsState;
use crate::context::world_state::WorldState;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::WaitForEnvironmentHandler;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

fn capture_packet(name: &str, value: serde_json::Value) {
    tracing::info!(name, value = %value, "AUDIT21_PACKET");
}

async fn failed_primary(turn: &mut crate::session::turn_context::TurnContext, cause: &str) {
    let ready = &mut turn.environments.turn_environments[0];
    ready.environment_id = "secondary".into();
    let resolution = futures::future::ready(Err(Arc::new(ExecServerError::Protocol(cause.into()))))
        .boxed()
        .shared();
    assert!(resolution.clone().await.is_err());
    turn.environments.starting = vec![StartingTurnEnvironment {
        environment: Arc::clone(&ready.environment),
        selection: TurnEnvironmentSelection {
            environment_id: "primary".into(),
            cwd: ready.cwd().clone(),
        },
        selection_index: 0,
        resolution,
        _lifecycle: None,
        snapshot_cleanup_tasks: None,
    }];
}

#[tokio::test]
async fn audit21_failed_primary_context_preserves_explicit_secondary_identity() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .finish(),
    );
    let (_session, mut turn) = make_session_and_context().await;
    failed_primary(&mut turn, "startup refused").await;
    let environments = &turn.environments;
    let cancellation = CancellationToken::new();
    assert!(
        wait_for_tool_environment(environments, None, &cancellation)
            .await
            .is_err()
    );
    assert_eq!(
        wait_for_tool_environment(environments, Some("secondary"), &cancellation)
            .await
            .unwrap()
            .unwrap()
            .environment_id,
        "secondary"
    );

    let mut world = WorldState::default();
    world.add_section(EnvironmentsState::from_turn_context_with_environments(
        &turn,
        environments,
    ));
    let (fragments, snapshot) = world.render_history_diff_with_snapshot(None, &[]);
    let rendered = fragments
        .iter()
        .map(|fragment| fragment.render())
        .collect::<String>();
    // The old projection omitted failed selections before rendering. Reproduce
    // that exact input transformation without swapping shared production files.
    let mut baseline_environments = environments.clone();
    baseline_environments.starting.clear();
    let baseline =
        EnvironmentsState::from_turn_context_with_environments(&turn, &baseline_environments)
            .render();
    capture_packet(
        "context",
        serde_json::json!({"baseline": baseline, "candidate": rendered}),
    );
    tracing::info!(context = %rendered, "AUDIT21_CONTEXT");
    assert!(
        rendered.contains("id=\"secondary\""),
        "ready secondary identity is required for dispatch: {rendered}"
    );
    assert!(rendered.contains("id=\"primary\""));
    assert!(rendered.contains("<status>failed</status>"));
    assert!(!rendered.contains("<status>starting</status>"));
    let history = fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    let persisted = serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
    assert!(
        world
            .render_history_diff_with_snapshot(Some(&persisted), &history)
            .0
            .is_empty()
    );
    let (restored, _) = world.render_history_diff_with_snapshot(Some(&persisted), &[]);
    let restored = restored
        .iter()
        .map(|fragment| fragment.render())
        .collect::<String>();
    assert!(restored.contains("id=\"secondary\""));
    assert!(restored.contains("<status>failed</status>"));

    let mut recovered = turn.environments.turn_environments[0].clone();
    recovered.environment_id = "primary".into();
    turn.environments.turn_environments.insert(0, recovered);
    turn.environments.starting.clear();
    let mut recovered_world = WorldState::default();
    recovered_world.add_section(EnvironmentsState::from_turn_context_with_environments(
        &turn,
        &turn.environments,
    ));
    let (recovered, _) =
        recovered_world.render_history_diff_with_snapshot(Some(&persisted), &history);
    let recovered = recovered
        .iter()
        .map(|fragment| fragment.render())
        .collect::<String>();
    assert!(
        recovered.contains("<status>available</status>"),
        "recovery from failure is explicit"
    );
    assert!(!recovered.contains("<status>failed</status>"));
}

#[tokio::test]
async fn audit21_wait_failure_preserves_owner_cause_without_changing_task_scope() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .finish(),
    );
    let (session, mut turn) = make_session_and_context().await;
    let cause = "invalid shell for environment `primary`: unsupported shell /bin/fictional";
    failed_primary(&mut turn, cause).await;
    let payload = ToolPayload::Function {
        arguments: r#"{"environment_id":"primary"}"#.into(),
    };
    let output = WaitForEnvironmentHandler
        .handle(ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::new(turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "audit21-wait".into(),
            tool_name: ToolName::plain("wait_for_environment"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .unwrap();
    assert!(!output.success_for_logging());
    let result = output.code_mode_result(&payload);
    capture_packet(
        "wait",
        serde_json::json!({
            "baseline": "Environment `primary` failed to start and is unavailable. Continue without it.",
            "candidate": result, "cause": cause,
        }),
    );
    tracing::info!(result = %result, "AUDIT21_WAIT");
    assert!(
        result.to_string().contains(cause),
        "startup cause must reach the caller: {result}"
    );
    assert!(
        !result.to_string().contains("Continue without it"),
        "failure is not authority to drop required work"
    );
}

#[tokio::test]
async fn audit21_mixed_readiness_keeps_environment_selection_in_tool_contracts() {
    let (_session, mut turn) = make_session_and_context().await;
    failed_primary(&mut turn, "startup refused").await;
    turn.model_info.supports_search_tool = false;
    turn.model_info.shell_type = codex_protocol::openai_models::ConfigShellToolType::UnifiedExec;
    turn.model_info.apply_patch_tool_type =
        Some(codex_protocol::openai_models::ApplyPatchToolType::Freeform);
    let step = StepContext::for_test(Arc::new(turn));
    let cache = crate::tools::handlers::ToolSearchHandlerCache::default();
    let router = crate::tools::router::ToolRouter::from_context(
        &step,
        crate::tools::router::ToolRouterParams {
            mcp_tools: None,
            deferred_mcp_tools: None,
            tool_suggest_candidates: None,
            extension_tool_executors: vec![],
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &cache,
    );
    let specs = router.model_visible_specs();
    let exec = specs
        .iter()
        .find(|spec| spec.name() == "exec_command")
        .expect("ready secondary has exec");
    let ToolSpec::Function(exec) = exec else {
        panic!("exec is a function")
    };
    assert!(
        exec.parameters
            .properties
            .as_ref()
            .unwrap()
            .contains_key("environment_id"),
        "the ready secondary must be selectable even when the primary failed"
    );
    let patch = specs
        .iter()
        .find(|spec| spec.name() == "apply_patch")
        .expect("patch tool");
    assert!(
        serde_json::to_string(patch)
            .unwrap()
            .contains("environment_id")
    );
}
