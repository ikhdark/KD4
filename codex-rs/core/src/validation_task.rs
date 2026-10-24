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

pub(crate) fn policy_limit(turn: &TurnContext) -> usize {
    use codex_protocol::validation::ValidationMode;
    match turn.validation_mode {
        ValidationMode::Off => 0,
        ValidationMode::Compile => 2,
        ValidationMode::Benchmark | ValidationMode::Full => turn.validation_full_budget,
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
            "COMPILE permits only targeted cargo check, not tests, gates, formatting, Clippy, or full builds. No command was launched.",
        ValidationMode::Benchmark if !matches!(classification, Class::TargetedCompilation | Class::Benchmark) =>
            "BENCHMARK permits only targeted cargo check and recognized benchmark-only workloads, not tests, mixed or unclassified validation, gates, formatting, Clippy, or full builds. No command was launched.",
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
    // Missing source identity must not grant unlimited retries. This sentinel is
    // deliberately stable, and cannot be used as verification evidence.
    let revision = source_revision.unwrap_or("source-identity-unavailable").to_owned();
    turn.full_suite_budget.reserve_policy(
        reservation_owner(session, turn, call_id),
        serde_json::json!([execution_scope, selection]).to_string(), revision,
        policy_limit(turn), turn.validation_mode == codex_protocol::validation::ValidationMode::Compile
            || (turn.validation_mode == codex_protocol::validation::ValidationMode::Benchmark
                && classification == Class::TargetedCompilation),
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
        assert_eq!(ctx.turn.full_suite_budget.policy_snapshot(2).admitted, 1);
        assert!(begin_policy_attempt(&ctx).await.is_err());
        policy_attempt_not_started(&ctx).await;
        assert_eq!(ctx.turn.full_suite_budget.policy_snapshot(2).admitted, 0);
        begin_policy_attempt(&ctx).await.unwrap();
        assert!(begin_policy_attempt(&ctx).await.is_err());
        drop(permit);
        assert_eq!(ctx.turn.full_suite_budget.policy_snapshot(2).admitted, 1);
    }
}
