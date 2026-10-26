//! Per-turn validation mode policy.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::sandboxing::ToolCtx;

/// A policy refusal is not an execution failure or a busy validation lane.
pub(crate) fn policy_refusal(
    turn: &TurnContext,
    reason: impl Into<String>,
) -> crate::tools::context::FunctionToolOutput {
    let signal = serde_json::json!({
        "validation_policy_refusal": true,
        "mode": turn.validation_mode,
        "execution_state": "not_started",
        "status": "unvalidated",
        "reason": reason.into(),
    });
    let mut output = crate::tools::context::FunctionToolOutput::from_text(
        signal.to_string(), Some(false),
    ).with_sampling_request_signal(signal.clone());
    output.essential_inline = signal.as_object().cloned().unwrap_or_default();
    output
}

/// Runtime/tooling allowance; targeted compilation is admitted separately.
pub(crate) fn policy_limit(turn: &TurnContext) -> usize {
    use codex_protocol::validation::ValidationMode;
    match turn.validation_mode {
        ValidationMode::Off | ValidationMode::Compile => 0,
        ValidationMode::FullHalf | ValidationMode::Full => turn.validation_full_budget,
    }
}

pub(crate) fn mode_refusal(
    turn: &TurnContext,
    classification: codex_command_analysis::validation::ValidationPolicyClassification,
    declared: bool,
) -> Option<crate::tools::context::FunctionToolOutput> {
    use codex_command_analysis::validation::ValidationPolicyClassification as Class;
    use codex_protocol::validation::ValidationMode;
    if classification == Class::ReadOnly { return None; }
    let recognized = classification.is_validation();
    if !recognized && !declared { return None; }
    let reason = match turn.validation_mode {
        ValidationMode::Off => "Validation is disabled for this turn. No command was launched; report unvalidated rather than retrying.",
        ValidationMode::Compile if classification != Class::TargetedCompilation =>
            "COMPILE permits only targeted compile checks, not tests, gates, formatting, non-compilation checks, or full builds. No command was launched.",
        ValidationMode::FullHalf if matches!(classification, Class::Tests | Class::Checks) =>
            "Full .5 allows compile checks and other task-authorized operations, but not tests, full suites, or non-compilation checks in any language. Select FULL explicitly for those checks or tests. No command was launched.",
        _ => return None,
    };
    Some(policy_refusal(turn, reason))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn reserve_policy(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    classification: codex_command_analysis::validation::ValidationPolicyClassification,
    declared: bool,
    selection: String,
    execution_scope: &str,
    source_revision: Option<&str>,
) -> Result<Option<codex_command_analysis::command_preflight::FullSuitePermit>, crate::tools::context::FunctionToolOutput> {
    use codex_command_analysis::validation::ValidationPolicyClassification as Class;
    if classification == Class::ReadOnly { return Ok(None); }
    if let Some(refusal) = mode_refusal(turn, classification, declared) { return Err(refusal); }
    if !declared && !classification.is_validation() {
        return Ok(None);
    }
    // Count executions, not shell envelopes: a compound script must not hide
    // multiple checks in one reservation. A runner's own batch is one leaf.
    if serde_json::from_str::<Vec<Vec<String>>>(&selection).is_ok_and(|commands| {
        commands.iter().filter(|argv| argv.split_first().is_some_and(|(program, args)| {
            crate::validation::classify_validation_policy(
                &crate::tools::handlers::command_shape::CommandInvocation::Argv {
                    program: program.clone(), args: args.to_vec(),
                },
            ).is_validation()
        })).count() > 1
    }) {
        return Err(policy_refusal(turn, "Multiple validation launches in one shell command are not permitted; use one targeted check or one authorized runner batch."));
    }
    // Detached worktrees and disabled workspace capture can lack a content
    // fingerprint. Do not collapse their tracked source edits to one sentinel.
    // This fallback invalidates admission only; it is not source-byte evidence.
    let revision = match source_revision {
        Some(revision) => revision.to_owned(),
        None => {
            // Both real command handlers already observed the tracker revision.
            // Observing zero reads that monotonic epoch without invalidating it.
            let epoch = session.services.command_execution
                .observe_repository_revision(&turn.sub_id, 0).await;
            format!("tracked-source-epoch:{epoch}")
        }
    };
    turn.full_suite_budget.reserve_policy(
        reservation_owner(session, turn, call_id),
        serde_json::json!([execution_scope, selection]).to_string(), revision,
        policy_limit(turn), classification == Class::TargetedCompilation,
    ).map(Some).map_err(|reason| policy_refusal(turn, reason))
}

pub(crate) fn reservation_owner(session: &Session, turn: &TurnContext, call: &str) -> String {
    format!("{}:{}:{call}", session.thread_id(), turn.sub_id)
}

pub(crate) async fn acquire_execution_resources(
    ctx: &ToolCtx,
    command: &[String],
    cwd: Option<&std::path::Path>,
) -> Result<Option<crate::validation_resources::ValidationResourceLease>, String> {
    let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
    if !ctx.turn.full_suite_budget.reserved_for(&owner)
        && !ctx.turn.full_suite_budget.started_for(&owner)
    {
        return Ok(None);
    }
    let Some(cwd) = cwd else { return Ok(None); };
    crate::validation_resources::acquire_python_test_resource(command, cwd).await
}

/// Checkpoint consumption before launch. Only proven nonexecution permits retry.
pub(crate) async fn begin_policy_attempt(ctx: &ToolCtx) -> Result<(), String> {
    let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
    let budget = &ctx.turn.full_suite_budget;
    if !budget.reserved_for(&owner) && !budget.started_for(&owner) {
        return Ok(());
    }
    let _checkpoint = budget.checkpoint_guard().await;
    if budget.started_for(&owner) {
        return Err("Validation already crossed the possible-start boundary for this call; retain its result or resume its live process instead of relaunching.".into());
    }
    if !budget.reserved_for(&owner) {
        return Ok(());
    }
    budget.start(&owner, None)?;
    if let Err(error) = persist_policy_checkpoint(&ctx.session, &ctx.turn).await {
        budget.acknowledge_not_spawned(&owner);
        return Err(error);
    }
    Ok(())
}

pub(crate) async fn policy_attempt_not_started(ctx: &ToolCtx) {
    let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
    if !ctx.turn.full_suite_budget.started_for(&owner) {
        return;
    }
    let _checkpoint = ctx.turn.full_suite_budget.checkpoint_guard().await;
    ctx.turn.full_suite_budget.acknowledge_not_spawned(
        &owner,
    );
    let _ = persist_policy_checkpoint(&ctx.session, &ctx.turn).await;
}

async fn persist_policy_checkpoint(session: &Session, turn: &TurnContext) -> Result<(), String> {
    session.services.agent_control.persist_root_validation_checkpoint(turn).await?;
    session.persist_rollout_items_durable(&[
        codex_protocol::protocol::RolloutItem::TurnContext(turn.to_turn_context_item()),
    ]).await.map_err(|error| format!("validation admission checkpoint failed before launch: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn full_half_admission_preserves_test_and_check_restrictions_and_attempt_limit() {
        use codex_command_analysis::validation::ValidationPolicyClassification as Class;
        use codex_protocol::validation::ValidationMode as Mode;
        let (_, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.validation_full_budget = 2;
        for (mode, tooling_refused) in [
            (Mode::Off, true), (Mode::Compile, true),
            (Mode::FullHalf, false), (Mode::Full, false),
        ] {
            turn.validation_mode = mode;
            assert_eq!(mode_refusal(&turn, Class::Tooling, false).is_some(), tooling_refused);
            assert_eq!(mode_refusal(&turn, Class::Tooling, true).is_some(), tooling_refused);
        }
        turn.validation_mode = Mode::FullHalf;
        assert!(mode_refusal(&turn, Class::Benchmark, false).is_none());
        assert!(mode_refusal(&turn, Class::Restricted, false).is_none());
        assert!(mode_refusal(&turn, Class::Opaque, true).is_none());
        assert!(mode_refusal(&turn, Class::Tests, false).is_some());
        assert!(mode_refusal(&turn, Class::Tests, true).is_some());
        assert!(mode_refusal(&turn, Class::Checks, false).is_some());
        assert!(mode_refusal(&turn, Class::TargetedCompilation, true).is_none());
        assert_eq!(policy_limit(&turn), 2);
        turn.validation_mode = Mode::Full;
        assert!(mode_refusal(&turn, Class::Tests, false).is_none());
        assert!(mode_refusal(&turn, Class::Checks, true).is_none());
    }

    #[tokio::test]
    async fn dispatch_retry_requires_proven_nonexecution_and_keeps_budget_bounded() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let ctx = ToolCtx {
            session: Arc::new(session), turn: Arc::new(turn), call_id: "compile".into(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
        };
        let owner = reservation_owner(&ctx.session, &ctx.turn, &ctx.call_id);
        let permit = ctx.turn.full_suite_budget.reserve_policy(
            owner, "cargo check -p example".into(), "source".into(), 2, true,
        ).unwrap();
        begin_policy_attempt(&ctx).await.unwrap();
        assert_eq!(ctx.turn.full_suite_budget.policy_snapshot(2).admitted, 0);
        assert_eq!(ctx.turn.full_suite_budget.persisted_attempts().len(), 1);
        assert!(begin_policy_attempt(&ctx).await.is_err());
        policy_attempt_not_started(&ctx).await;
        assert_eq!(ctx.turn.full_suite_budget.policy_snapshot(2).admitted, 0);
        assert!(ctx.turn.full_suite_budget.persisted_attempts().is_empty());
        begin_policy_attempt(&ctx).await.unwrap();
        assert!(begin_policy_attempt(&ctx).await.is_err());
        drop(permit);
        assert_eq!(ctx.turn.full_suite_budget.policy_snapshot(2).admitted, 0);
        assert_eq!(ctx.turn.full_suite_budget.persisted_attempts().len(), 1);
    }

    #[tokio::test]
    async fn tracked_port_reopens_benchmark_admission_without_a_workspace_fingerprint() {
        use codex_command_analysis::validation::ValidationPolicyClassification as Class;
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.validation_mode = codex_protocol::validation::ValidationMode::FullHalf;
        turn.validation_full_budget = 2;
        let selection = serde_json::json!([
            ["cargo", "bench", "-p", "codex-core-skills", "--bench", "upstream_recovery"]
        ]).to_string();
        let scope = "detached-verification-worktree";
        let root = tempfile::tempdir().unwrap();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(root.path()).unwrap();
        let mut tracker = crate::turn_diff_tracker::TurnDiffTracker::new();
        let baseline_epoch = session.services.command_execution
            .observe_repository_revision(&turn.sub_id, tracker.current_mutation_revision()).await;
        let baseline = reserve_policy(&session, &turn, "baseline", Class::Benchmark,
            false, selection.clone(), scope, None,
        ).await.unwrap_or_else(|_| panic!("baseline must be admitted"));
        turn.full_suite_budget.start(&reservation_owner(&session, &turn, "baseline"), None).unwrap();
        drop(baseline);
        assert!(reserve_policy(&session, &turn, "duplicate", Class::Benchmark,
            false, selection.clone(), scope, None,
        ).await.is_err(), "unchanged tracked inputs must still be rejected");
        let delta = codex_apply_patch::apply_patch(
            "*** Begin Patch\n*** Add File: service.rs\n+// ported source\n*** End Patch",
            &cwd, &mut Vec::new(), &mut Vec::new(),
            codex_exec_server::LOCAL_FS.as_ref(), None,
        ).await.unwrap();
        tracker.track_delta(codex_exec_server::LOCAL_ENVIRONMENT_ID, &delta);
        let post_port_epoch = session.services.command_execution
            .observe_repository_revision(&turn.sub_id, tracker.current_mutation_revision()).await;
        assert_ne!(baseline_epoch, post_port_epoch);
        let post_port = reserve_policy(&session, &turn, "post-port", Class::Benchmark,
            false, selection, scope, None,
        ).await.unwrap_or_else(|_| panic!("tracked port must reopen admission"));
        turn.full_suite_budget.start(&reservation_owner(&session, &turn, "post-port"), None).unwrap();
        drop(post_port);
        assert_eq!(turn.full_suite_budget.policy_snapshot(2).admitted, 2);
    }

    #[tokio::test]
    async fn full_mode_compile_repairs_do_not_use_the_runtime_allowance() {
        use codex_command_analysis::validation::ValidationPolicyClassification as Class;
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.validation_mode = codex_protocol::validation::ValidationMode::Full;
        turn.validation_full_budget = 0;
        for index in 0..4 {
            let owner = format!("repair-{index}");
            let revision = format!("source-{index}");
            let permit = reserve_policy(&session, &turn, &owner, Class::TargetedCompilation,
                false, r#"[["cargo","check","-p","example"]]"#.into(), "workspace",
                Some(&revision),
            ).await.unwrap_or_else(|_| panic!("changed compile inputs remain admissible"));
            turn.full_suite_budget.start(&reservation_owner(&session, &turn, &owner), None).unwrap();
            drop(permit);
        }
        assert_eq!(turn.full_suite_budget.policy_snapshot(0).admitted, 0);
        assert_eq!(turn.full_suite_budget.persisted_attempts().len(), 4);
    }
}
