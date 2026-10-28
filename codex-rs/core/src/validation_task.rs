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
            "Full .5 allows compile checks, permitted non-test KD4 scripts workflows, and other task-authorized operations, but not tests/full test suites or other non-compilation checks in any language. Only the user can select FULL, by starting a new message with `/validation full` on its own first line; it lasts until their next message. No command was launched.",
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
    environment_id: &str,
    cwd: &codex_utils_path_uri::PathUri,
    command: &[String],
) -> Result<Option<codex_command_analysis::command_preflight::FullSuitePermit>, crate::tools::context::FunctionToolOutput> {
    use codex_command_analysis::validation::ValidationPolicyClassification as Class;
    if classification == Class::ReadOnly { return Ok(None); }
    if let Some(refusal) = mode_refusal(turn, classification, declared) { return Err(refusal); }
    if !declared && !classification.is_validation() {
        return Ok(None);
    }
    let revision = if classification == Class::TargetedCompilation {
        // Cache invalidation is not source-change evidence. Keep it for other
        // command observations, but use exact edits/named inputs for repairs.
        let source_selection = codex_command_analysis::validation::compilation_source_selection(&selection);
        let scope = serde_json::json!([environment_id, execution_scope, source_selection]).to_string();
        let unambiguous = command.split_first().is_some_and(|(program, args)|
            codex_shell_command::validation::validation_resource_cwd_is_unambiguous(program, args));
        let native_cwd = (environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID)
            .then(|| cwd.to_abs_path().ok()).flatten();
        let explicit_inputs = if !unambiguous { Some(Vec::new()) } else {
            codex_command_analysis::validation::explicit_compilation_inputs(&selection, command).map(|paths|
                paths.into_iter().map(|path| match native_cwd.as_ref() {
                    Some(base) => codex_utils_absolute_path::AbsolutePathBuf::resolve_literal_path_against_base(path, base)
                        .to_path_buf(),
                    None if path.has_root() => path,
                    None => std::path::PathBuf::from(cwd.inferred_native_path_string()).join(path),
                }).collect::<Vec<_>>())
        };
        let mut paths = explicit_inputs.clone().unwrap_or_default();
        let mut fingerprints = if native_cwd.is_some() && unambiguous {
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || crate::git_workspace::compilation_input_fingerprints(paths))
                .await.unwrap_or_default()
        } else { Default::default() };
        if explicit_inputs.is_none() && unambiguous
            && let Some(cwd) = native_cwd.as_ref()
        {
            let (selection, command, cwd) = (selection.clone(), command.to_vec(), cwd.to_path_buf());
            if let Ok(Some((named, current))) = tokio::task::spawn_blocking(move || {
                let cwd = codex_command_analysis::validation::compilation_cargo_cwd(&command, &cwd)?;
                let (manifests, roots) = codex_command_analysis::command_preflight::compilation_sources(&selection, &cwd)?;
                Some(crate::git_workspace::compilation_rust_inputs(manifests, roots))
            }).await {
                paths = named;
                fingerprints = current;
            }
        }
        turn.full_suite_budget.compilation_revision(
            &scope, environment_id, &paths, explicit_inputs.is_some(), fingerprints,
        )
    } else {
        let epoch = session.services.command_execution
            .observe_repository_revision(&turn.sub_id, 0).await;
        serde_json::json!([source_revision, epoch]).to_string()
    };
    turn.full_suite_budget.reserve_policy(
        reservation_owner(session, turn, call_id),
        serde_json::json!([execution_scope, selection]).to_string(), revision,
        classification == Class::TargetedCompilation,
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
