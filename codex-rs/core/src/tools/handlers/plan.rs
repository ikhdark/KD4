use crate::function_tool::FunctionCallError;
use crate::task_evidence::ArchitectureEvidenceFacetInput;
use crate::task_evidence::ArchitectureEvidenceProvenance;
use crate::task_evidence::ArchitectureEvidenceStatus;
use crate::task_evidence::ArchitectureExplorationMetricsInput;
use crate::task_evidence::ArchitectureSliceInput;
use crate::task_evidence::PlanStepEvidenceInput;
use crate::task_evidence::PlanUpdateEffect;
use crate::task_evidence::PlanningTier;
use crate::task_evidence::PlanningUpdateInput;
use crate::task_evidence::ValidationDisposition;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::plan_spec::create_update_plan_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::plan_tool::ValidationRoute;
use codex_protocol::plan_tool::ValidationRouteLeaf;
use codex_protocol::plan_tool::ValidationRouteOrdering;
use codex_protocol::protocol::EventMsg;
use codex_protocol::validation::ValidationTerminalStatus;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_tools::shell_command_backend_for_features;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
#[cfg(test)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use tokio::sync::Notify;

pub struct PlanHandler;

pub struct PlanToolOutput {
    normalized_plan: Option<UpdatePlanArgs>,
    governor_plan: Option<UpdatePlanArgs>,
    effect: PlanUpdateEffect,
    unfinished_mutation_obligation: Option<bool>,
    source_closure_established: bool,
    source_closure_receipt: Option<SourceClosureReceipt>,
    validation_results: Vec<JsonValue>,
    finalization_requested: bool,
    finalized: bool,
    missing_evidence: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct SourceClosureReceipt {
    established: bool,
    snapshot: Option<String>,
    expected_snapshot: Option<String>,
    stale_snapshot: bool,
    missing_requirements: Vec<String>,
    exact_relationships: u64,
    declared_relationships: u64,
    heuristic_relationships: u64,
    total_relationships: u64,
    invariant_relationships: u64,
    relationship_kinds: Vec<crate::task_evidence::ArchitectureRelationshipKind>,
    truncated: bool,
    omitted_relationships: u64,
    material_unknowns: Vec<String>,
    limitations: Vec<String>,
    metrics: ArchitectureExplorationMetricsInput,
}

const PLAN_UPDATED_MESSAGE: &str = "Plan updated";

impl PlanToolOutput {
    fn normalized_result(&self) -> JsonValue {
        let mut result = serde_json::json!({
            "message": PLAN_UPDATED_MESSAGE,
            "effect": self.effect.as_str(),
            "no_op": self.effect == PlanUpdateEffect::NoOp,
            "validation_results": self.validation_results,
            "finalization": {
                "requested": self.finalization_requested,
                "finalized": self.finalized,
                "missing_evidence": self.missing_evidence,
            },
        });
        if let Some(normalized_plan) = &self.normalized_plan {
            result["normalized_plan"] = serde_json::json!(normalized_plan);
        }
        if let Some(receipt) = &self.source_closure_receipt {
            result["source_closure"] = serde_json::json!(receipt);
        }
        result
    }
}

#[cfg(test)]
#[derive(Default)]
struct PlanCommitBoundaryHook {
    reached: Notify,
    release: Notify,
}

#[cfg(test)]
static PLAN_COMMIT_BOUNDARY_HOOKS: LazyLock<Mutex<HashMap<String, Arc<PlanCommitBoundaryHook>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
impl PlanCommitBoundaryHook {
    fn install(call_id: &str) -> Arc<Self> {
        let hook = Arc::new(Self::default());
        let previous = PLAN_COMMIT_BOUNDARY_HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call_id.to_string(), Arc::clone(&hook));
        assert!(
            previous.is_none(),
            "plan commit hook call IDs must be unique"
        );
        hook
    }

    async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

#[cfg(test)]
async fn pause_at_plan_commit_boundary(call_id: &str) {
    let hook = PLAN_COMMIT_BOUNDARY_HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(call_id);
    if let Some(hook) = hook {
        hook.reached.notify_one();
        hook.release.notified().await;
    }
}

impl ToolOutput for PlanToolOutput {
    fn log_preview(&self) -> String {
        PLAN_UPDATED_MESSAGE.to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        let source_closure = self.source_closure_receipt.as_ref().map(|receipt| {
            serde_json::json!({
                "missing_requirements": receipt.missing_requirements,
                "relationship_kinds": receipt.relationship_kinds,
                "total_relationships": receipt.total_relationships,
            })
        });
        Some(serde_json::json!({
            "kind": "plan_update",
            "plan": self.governor_plan,
            "unfinished_mutation_obligation": self.unfinished_mutation_obligation,
            "source_closure_established": self.source_closure_established,
            "source_closure": source_closure,
        }))
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let text = self.normalized_result().to_string();
        let mut output = FunctionCallOutputPayload::from_text(text);
        output.success = Some(true);

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.normalized_result()
    }
}

impl ToolExecutor<ToolInvocation> for PlanHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("update_plan")
    }

    fn spec(&self) -> ToolSpec {
        create_update_plan_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl PlanHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id: _call_id,
            source,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "update_plan handler received unsupported payload".to_string(),
                ));
            }
        };

        if turn.collaboration_mode.mode == ModeKind::Plan {
            return Err(FunctionCallError::RespondToModel(
                "update_plan is a TODO/checklist tool and is not allowed in Plan mode".to_string(),
            ));
        }

        let requested_input = parse_update_plan_arguments(&arguments)?;
        let finalization_requested = requested_input.finalize;
        let repository = codex_git_utils::get_git_repo_root(turn.config.cwd.as_path())
            .unwrap_or_else(|| turn.config.cwd.to_path_buf());
        validate_requested_validation_routes(&requested_input, &repository)?;
        if cancellation_token.is_cancelled() {
            return Err(FunctionCallError::RespondToModel(
                "update_plan was cancelled before the plan update began".to_string(),
            ));
        }
        #[cfg(test)]
        pause_at_plan_commit_boundary(&_call_id).await;
        let outcome = session
            .services
            .task_evidence
            .record_planning_update(requested_input.clone())
            .await;
        let mut closure_input = requested_input.clone();
        session
            .services
            .task_evidence
            .hydrate_planning_source_evidence(&mut closure_input)
            .await;
        let expected_snapshot = codex_git_utils::get_head_commit_hash(&repository)
            .await
            .map(|sha| sha.0);
        let (source_closure_established, source_closure_receipt, closure_surfaces) =
            planning_update_source_closure(&closure_input, expected_snapshot.as_deref());
        if source_closure_established {
            session
                .services
                .task_evidence
                .capture_source_closure_snapshot(&closure_surfaces)
                .await;
        }
        if let Some(receipt) = source_closure_receipt.as_ref() {
            turn.turn_timing_state.record_architecture_slice_evaluation(
                receipt.established,
                receipt.total_relationships,
                receipt.invariant_relationships,
                receipt.missing_requirements.len() as u64,
                receipt.material_unknowns.len() as u64,
                receipt.stale_snapshot,
                receipt.metrics.tool_calls,
                receipt.metrics.files_read,
                receipt.metrics.bytes_read,
                receipt.metrics.late_relationship_discoveries,
            );
        }
        match outcome.effect {
            PlanUpdateEffect::Initial => turn.turn_timing_state.record_initial_plan_generation(),
            PlanUpdateEffect::StructuralRevision => {
                turn.turn_timing_state.record_plan_revision_generation()
            }
            PlanUpdateEffect::StatusOnly | PlanUpdateEffect::NoOp => {}
        }
        let args = outcome.public_update;
        let requested_args = UpdatePlanArgs {
            explanation: requested_input.explanation,
            plan: requested_input.plan,
        };
        session
            .send_event(turn.as_ref(), EventMsg::PlanUpdate(args.clone()))
            .await;

        let mut validation_results = Vec::new();
        let max_validation_rounds = if finalization_requested {
            args.plan.len().saturating_add(1)
        } else {
            1
        };
        let mut previous_candidate = None;
        for round in 0..max_validation_rounds {
            let Some(candidate) = session
                .services
                .task_evidence
                .auto_validation_candidate()
                .await
            else {
                break;
            };
            let candidate_identity = (
                candidate.step_id.clone(),
                candidate.implementation_identity.clone(),
            );
            if previous_candidate.as_ref() == Some(&candidate_identity) {
                break;
            }
            previous_candidate = Some(candidate_identity);
            let round_results = run_bound_validation_route(
                session.clone(),
                turn.clone(),
                step_context.clone(),
                tracker.clone(),
                cancellation_token.clone(),
                source.clone(),
                &_call_id,
                candidate,
                repository.clone(),
            )
            .await;
            let successful = validation_round_succeeded(&round_results);
            validation_results.extend(round_results);
            if !finalization_requested || !successful || cancellation_token.is_cancelled() {
                break;
            }
            tracing::debug!(round, "update_plan finalization consumed validation round");
        }

        let mut final_plan = session
            .services
            .task_evidence
            .current_plan_update()
            .await
            .unwrap_or_else(|| args.clone());
        final_plan.explanation.clone_from(&args.explanation);
        if final_plan != args {
            session
                .send_event(turn.as_ref(), EventMsg::PlanUpdate(final_plan.clone()))
                .await;
        }
        let missing_evidence = finalization_missing_evidence(&final_plan);
        let finalized = finalization_requested && missing_evidence.is_empty();
        let normalized_plan = (final_plan != requested_args).then(|| final_plan.clone());

        Ok(boxed_tool_output(PlanToolOutput {
            normalized_plan,
            governor_plan: outcome.effect.requests_generation().then_some(final_plan),
            effect: outcome.effect,
            unfinished_mutation_obligation: outcome.unfinished_mutation_obligation,
            source_closure_established,
            source_closure_receipt,
            validation_results,
            finalization_requested,
            finalized,
            missing_evidence,
        }))
    }
}

fn validate_requested_validation_routes(
    requested_input: &PlanningUpdateInput,
    repository: &std::path::Path,
) -> Result<(), FunctionCallError> {
    if let Some(route) = requested_input.validation_route.as_ref() {
        validate_requested_validation_route(route, repository, "focused validation route")?;
    }
    for (step_index, item) in requested_input.plan.iter().enumerate() {
        let Some(route) = item.validation_route.as_ref() else {
            continue;
        };
        let owner = item.id.as_deref().map_or_else(
            || format!("plan step #{}", step_index + 1),
            |id| format!("plan step `{id}`"),
        );
        validate_requested_validation_route(route, repository, &owner)?;
    }
    Ok(())
}

fn validate_requested_validation_route(
    route: &ValidationRoute,
    repository: &std::path::Path,
    owner: &str,
) -> Result<(), FunctionCallError> {
    for (leaf_index, leaf) in route.leaves.iter().enumerate() {
        super::shell::validate_structured_validation_leaf(leaf, repository).map_err(|reason| {
            FunctionCallError::RespondToModel(format!(
                "{owner} leaf {} could not be bound: {reason}",
                leaf_index + 1
            ))
        })?;
    }
    Ok(())
}

fn validation_round_succeeded(results: &[JsonValue]) -> bool {
    results.iter().rev().find_map(|result| {
        result
            .get("aggregate")
            .and_then(JsonValue::as_bool)
            .filter(|aggregate| *aggregate)
            .map(|_| result.get("success").and_then(JsonValue::as_bool) == Some(true))
    }) == Some(true)
}

fn finalization_missing_evidence(plan: &UpdatePlanArgs) -> Vec<String> {
    plan.plan
        .iter()
        .filter_map(|item| {
            let id = item.id.as_deref().unwrap_or("unnamed");
            match &item.status {
                codex_protocol::plan_tool::StepStatus::Passed
                | codex_protocol::plan_tool::StepStatus::Skipped
                | codex_protocol::plan_tool::StepStatus::Completed => None,
                codex_protocol::plan_tool::StepStatus::Implemented
                    if item.validation_route.is_some() =>
                {
                    Some(format!(
                        "step {id}: missing fresh successful validation evidence"
                    ))
                }
                codex_protocol::plan_tool::StepStatus::Blocked => {
                    Some(format!("step {id}: blocked"))
                }
                status => Some(format!("step {id}: unfinished ({status:?})")),
            }
        })
        .collect()
}

const MAX_ARCHITECTURE_RELATIONSHIPS: u64 = 32;

fn planning_update_source_closure(
    input: &PlanningUpdateInput,
    expected_snapshot: Option<&str>,
) -> (bool, Option<SourceClosureReceipt>, Vec<String>) {
    if input.tier == Some(PlanningTier::Focused) || input.plan.is_empty() {
        let receipt = source_evidence_receipt(
            input.source_owner.as_deref(),
            &input.implementation_surfaces,
            input.validation_disposition,
            input.validation_route.is_some() || input.external_validation_route.is_some(),
            input.architecture_slice.as_ref(),
            expected_snapshot,
        );
        let report = input.source_owner.is_some()
            || !input.implementation_surfaces.is_empty()
            || input.architecture_slice.is_some();
        return (
            receipt.established,
            report.then_some(receipt),
            input.implementation_surfaces.clone(),
        );
    }

    let dependency_is_finished = |dependency: &str| {
        input.plan.iter().any(|item| {
            item.id.as_deref() == Some(dependency)
                && matches!(
                    item.status,
                    codex_protocol::plan_tool::StepStatus::Passed
                        | codex_protocol::plan_tool::StepStatus::Skipped
                        | codex_protocol::plan_tool::StepStatus::Completed
                )
        })
    };
    let mut fallback = None;
    for item in input.plan.iter().filter(|item| {
        matches!(
            item.status,
            codex_protocol::plan_tool::StepStatus::Pending
                | codex_protocol::plan_tool::StepStatus::InProgress
        ) && item
            .depends_on
            .iter()
            .all(|dependency| dependency_is_finished(dependency))
    }) {
        let Some(evidence) = input
            .step_evidence
            .iter()
            .find(|evidence| item.id.as_deref() == Some(evidence.step_id.as_str()))
        else {
            continue;
        };
        let receipt = step_source_evidence_receipt(
            evidence,
            item.validation_route.is_some(),
            expected_snapshot,
        );
        if receipt.established {
            return (
                true,
                Some(receipt),
                evidence.implementation_surfaces.clone(),
            );
        }
        fallback.get_or_insert((receipt, evidence.implementation_surfaces.clone()));
    }
    fallback.map_or_else(
        || (false, None, Vec::new()),
        |(receipt, surfaces)| (false, Some(receipt), surfaces),
    )
}

fn step_source_evidence_receipt(
    evidence: &PlanStepEvidenceInput,
    has_inline_validation_route: bool,
    expected_snapshot: Option<&str>,
) -> SourceClosureReceipt {
    source_evidence_receipt(
        evidence.source_owner.as_deref(),
        &evidence.implementation_surfaces,
        evidence.validation_disposition,
        has_inline_validation_route || evidence.external_validation_route.is_some(),
        evidence.architecture_slice.as_ref(),
        expected_snapshot,
    )
}

fn source_evidence_receipt(
    source_owner: Option<&str>,
    implementation_surfaces: &[String],
    validation_disposition: Option<ValidationDisposition>,
    has_validation_route: bool,
    architecture_slice: Option<&ArchitectureSliceInput>,
    expected_snapshot: Option<&str>,
) -> SourceClosureReceipt {
    let mut receipt = architecture_slice.map_or_else(SourceClosureReceipt::default, |slice| {
        architecture_slice_receipt(slice, expected_snapshot)
    });
    if source_owner.is_none_or(|owner| owner.trim().is_empty()) {
        receipt
            .missing_requirements
            .push("source_owner".to_string());
    }
    if implementation_surfaces.is_empty()
        || implementation_surfaces
            .iter()
            .any(|surface| surface.trim().is_empty())
    {
        receipt
            .missing_requirements
            .push("implementation_surfaces".to_string());
    }
    match validation_disposition {
        Some(ValidationDisposition::Executable) if !has_validation_route => receipt
            .missing_requirements
            .push("validation_route".to_string()),
        Some(ValidationDisposition::NotRequired | ValidationDisposition::UnavailableBlocked) => {}
        Some(ValidationDisposition::Executable) => {}
        Some(ValidationDisposition::UnresolvedDiscoverable) | None => receipt
            .missing_requirements
            .push("resolved_validation_disposition".to_string()),
    }
    if architecture_slice.is_none() {
        receipt
            .missing_requirements
            .push("architecture_slice".to_string());
    }
    receipt.missing_requirements.sort();
    receipt.missing_requirements.dedup();
    receipt.established = receipt.missing_requirements.is_empty();
    receipt
}

fn architecture_slice_receipt(
    slice: &ArchitectureSliceInput,
    expected_snapshot: Option<&str>,
) -> SourceClosureReceipt {
    let facets = [
        ("control_and_data_flow", &slice.control_and_data_flow),
        ("callers_and_consumers", &slice.callers_and_consumers),
        ("configuration_and_gates", &slice.configuration_and_gates),
        (
            "registration_and_entrypoints",
            &slice.registration_and_entrypoints,
        ),
        ("tests_and_contracts", &slice.tests_and_contracts),
        ("generated_artifacts", &slice.generated_artifacts),
        ("invariants", &slice.invariants),
    ];
    let mut receipt = SourceClosureReceipt {
        snapshot: (!slice.snapshot.trim().is_empty()).then(|| slice.snapshot.clone()),
        expected_snapshot: expected_snapshot.map(str::to_string),
        stale_snapshot: expected_snapshot
            .is_some_and(|expected| architecture_snapshot_revision(&slice.snapshot) != expected),
        truncated: slice.truncated,
        omitted_relationships: slice.omitted_relationships,
        material_unknowns: slice.material_unknowns.clone(),
        limitations: slice.limitations.clone(),
        metrics: slice.metrics.clone(),
        ..Default::default()
    };
    if receipt.snapshot.is_none() {
        receipt
            .missing_requirements
            .push("architecture_slice.snapshot".to_string());
    }
    if receipt.stale_snapshot {
        receipt
            .missing_requirements
            .push("architecture_slice.current_snapshot".to_string());
    }
    if slice.truncated {
        receipt
            .missing_requirements
            .push("architecture_slice.not_truncated".to_string());
    }
    if slice.omitted_relationships != 0 {
        receipt
            .missing_requirements
            .push("architecture_slice.zero_omissions".to_string());
    }
    if !slice.material_unknowns.is_empty() {
        receipt
            .missing_requirements
            .push("architecture_slice.zero_material_unknowns".to_string());
    }
    for (name, facet) in facets {
        count_relationship_provenance(&mut receipt, facet);
        if name == "invariants" {
            receipt.invariant_relationships = facet.relationships.len() as u64;
        }
        if !architecture_facet_is_closed(facet) {
            receipt
                .missing_requirements
                .push(format!("architecture_slice.{name}"));
        }
    }
    if receipt.total_relationships > MAX_ARCHITECTURE_RELATIONSHIPS {
        receipt
            .missing_requirements
            .push("architecture_slice.relationship_budget".to_string());
    }
    receipt.relationship_kinds.sort();
    receipt.relationship_kinds.dedup();
    receipt.established = receipt.missing_requirements.is_empty();
    receipt
}

fn architecture_snapshot_revision(snapshot: &str) -> &str {
    snapshot
        .split_once(':')
        .map_or(snapshot, |(revision, _)| revision)
}

fn architecture_facet_is_closed(facet: &ArchitectureEvidenceFacetInput) -> bool {
    match facet.status {
        ArchitectureEvidenceStatus::Established => {
            !facet.relationships.is_empty()
                && facet.relationships.iter().all(|relationship| {
                    !relationship.source.trim().is_empty()
                        && !relationship.target.trim().is_empty()
                        && !relationship.evidence.trim().is_empty()
                })
                && facet.relationships.iter().any(|relationship| {
                    matches!(
                        relationship.provenance,
                        ArchitectureEvidenceProvenance::Exact
                            | ArchitectureEvidenceProvenance::Declared
                    )
                })
        }
        ArchitectureEvidenceStatus::NotApplicable => {
            facet.relationships.is_empty()
                && facet
                    .not_applicable_reason
                    .as_deref()
                    .is_some_and(|reason| !reason.trim().is_empty())
        }
    }
}

fn count_relationship_provenance(
    receipt: &mut SourceClosureReceipt,
    facet: &ArchitectureEvidenceFacetInput,
) {
    for relationship in &facet.relationships {
        receipt.total_relationships += 1;
        receipt.relationship_kinds.push(relationship.kind);
        match relationship.provenance {
            ArchitectureEvidenceProvenance::Exact => receipt.exact_relationships += 1,
            ArchitectureEvidenceProvenance::Declared => receipt.declared_relationships += 1,
            ArchitectureEvidenceProvenance::Heuristic => receipt.heuristic_relationships += 1,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_bound_validation_route(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    step_context: Arc<crate::session::step_context::StepContext>,
    tracker: SharedTurnDiffTracker,
    cancellation_token: tokio_util::sync::CancellationToken,
    source: ToolCallSource,
    parent_call_id: &str,
    candidate: crate::task_evidence::AutoValidationCandidate,
    repository: PathBuf,
) -> Vec<JsonValue> {
    let mut results = Vec::with_capacity(candidate.route.leaves.len());
    let route_id = stable_validation_route_id(&candidate.route);
    let route_started_at = std::time::Instant::now();
    if candidate.route.ordering == ValidationRouteOrdering::RunAll {
        let executions = candidate
            .route
            .leaves
            .iter()
            .enumerate()
            .map(|(index, leaf)| {
                run_bound_validation_leaf(
                    session.clone(),
                    turn.clone(),
                    step_context.clone(),
                    tracker.clone(),
                    cancellation_token.clone(),
                    source.clone(),
                    parent_call_id,
                    &repository,
                    &candidate,
                    index,
                    leaf,
                )
            });
        results.extend(
            futures::future::join_all(executions)
                .await
                .into_iter()
                .flatten(),
        );
    } else {
        for (index, leaf) in candidate.route.leaves.iter().enumerate() {
            let Some(result) = run_bound_validation_leaf(
                session.clone(),
                turn.clone(),
                step_context.clone(),
                tracker.clone(),
                cancellation_token.clone(),
                source.clone(),
                parent_call_id,
                &repository,
                &candidate,
                index,
                leaf,
            )
            .await
            else {
                break;
            };
            let success = super::shell::ValidationExecutionOutcome::from_value(&result)
                == Some(super::shell::ValidationExecutionOutcome::ExecutedSuccess);
            results.push(result);
            if !success {
                break;
            }
        }
    }
    if !results.is_empty() {
        let aggregate_outcome = if results.iter().any(|result| {
            super::shell::ValidationExecutionOutcome::from_value(result)
                == Some(super::shell::ValidationExecutionOutcome::ExecutedFailure)
        }) {
            super::shell::ValidationExecutionOutcome::ExecutedFailure
        } else if results.iter().all(|result| {
            super::shell::ValidationExecutionOutcome::from_value(result)
                == Some(super::shell::ValidationExecutionOutcome::ExecutedSuccess)
        }) {
            super::shell::ValidationExecutionOutcome::ExecutedSuccess
        } else {
            super::shell::ValidationExecutionOutcome::NotExecuted
        };
        let completed_leaf_count = results.len();
        results.push(serde_json::json!({
            "step_id": candidate.step_id,
            "route_id": route_id,
            "aggregate": true,
            "ordering": candidate.route.ordering,
            "declared_leaf_count": candidate.route.leaves.len(),
            "completed_leaf_count": completed_leaf_count,
            "success": aggregate_outcome.success(),
            "execution_outcome": aggregate_outcome.as_str(),
            "command_was_executed": aggregate_outcome != super::shell::ValidationExecutionOutcome::NotExecuted,
            "duration_ms": u64::try_from(route_started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        }));
    }
    results
}

fn stable_validation_route_id(route: &ValidationRoute) -> String {
    let encoded = serde_json::to_vec(route).unwrap_or_else(|error| error.to_string().into_bytes());
    let digest = Sha256::digest(encoded);
    format!("validation-route-v1:{digest:x}")
}

#[allow(clippy::too_many_arguments)]
async fn run_bound_validation_leaf(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    step_context: Arc<crate::session::step_context::StepContext>,
    tracker: SharedTurnDiffTracker,
    cancellation_token: tokio_util::sync::CancellationToken,
    source: ToolCallSource,
    parent_call_id: &str,
    repository: &std::path::Path,
    candidate: &crate::task_evidence::AutoValidationCandidate,
    index: usize,
    leaf: &ValidationRouteLeaf,
) -> Option<JsonValue> {
    if cancellation_token.is_cancelled() {
        return None;
    }
    // Re-read the exact route immediately before every start/join. A newer
    // edit or plan change supersedes the pending launch without rediscovery.
    let current = session
        .services
        .task_evidence
        .auto_validation_candidate()
        .await;
    if current.as_ref().is_none_or(|current| {
        current.step_id != candidate.step_id
            || current.implementation_identity != candidate.implementation_identity
            || current.route != candidate.route
    }) {
        tracing::info!(step_id = %candidate.step_id, "auto-validation launch superseded");
        return None;
    }
    if let Err(reason) = super::shell::validate_structured_validation_leaf(leaf, repository) {
        tracing::info!(%reason, step_id = %candidate.step_id, "auto-validation fresh admission rejected");
        return None;
    }
    let (program, argv) = leaf.argv.split_first()?;
    let synthetic_call_id = format!("{parent_call_id}:validation:{index}");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": program,
            "args": argv,
            "timeout_ms": leaf.timeout_ms,
            "workdir": repository,
        })
        .to_string(),
    };
    let started_at = std::time::Instant::now();
    let invocation = ToolInvocation {
        session: session.clone(),
        turn: turn.clone(),
        step_context: step_context.clone(),
        cancellation_token: cancellation_token.clone(),
        tracker: tracker.clone(),
        call_id: synthetic_call_id.clone(),
        tool_name: ToolName::plain("shell_command"),
        source: source.clone(),
        payload: payload.clone(),
    };
    let handler =
        super::shell::ShellCommandHandler::new(super::shell::ShellCommandHandlerOptions {
            backend_config: shell_command_backend_for_features(turn.config.features.get()),
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
        });
    let bound = crate::tools::command_execution::BoundAutoValidationLeaf {
        step_id: candidate.step_id.clone(),
        implementation_revision: candidate.implementation_revision,
        implementation_identity: candidate.implementation_identity.clone(),
        repository: repository.to_path_buf(),
        route: candidate.route.clone(),
        leaf_index: index,
    };
    if !session
        .services
        .command_execution
        .bind_auto_validation_leaf(synthetic_call_id.clone(), bound)
        .await
    {
        tracing::warn!(%synthetic_call_id, "duplicate auto-validation call binding");
        return None;
    }
    let result = handler.handle_call(invocation).await;
    session
        .services
        .command_execution
        .clear_auto_validation_leaf(&synthetic_call_id)
        .await;
    let current = session
        .services
        .task_evidence
        .auto_validation_candidate()
        .await;
    let superseded = current.as_ref().is_none_or(|current| {
        current.step_id != candidate.step_id
            || current.implementation_identity != candidate.implementation_identity
            || current.route != candidate.route
    });
    let settled = if superseded {
        session
            .services
            .command_execution
            .supersede_validation_result_for_call(&synthetic_call_id)
            .await
    } else {
        session
            .services
            .command_execution
            .validation_result_for_call(&synthetic_call_id)
            .await
    };
    let (execution_outcome, output) = match result {
        Ok(output) => {
            if let Some(settled) = settled {
                let execution_outcome = match settled.status {
                    ValidationTerminalStatus::Succeeded => {
                        super::shell::ValidationExecutionOutcome::ExecutedSuccess
                    }
                    ValidationTerminalStatus::Superseded => {
                        super::shell::ValidationExecutionOutcome::NotExecuted
                    }
                    ValidationTerminalStatus::Failed => {
                        super::shell::ValidationExecutionOutcome::ExecutedFailure
                    }
                };
                (
                    execution_outcome,
                    serde_json::json!({ "validation_result": settled }),
                )
            } else {
                let execution_outcome = match output.outcome_context().outcome {
                    codex_tools::ToolOutputOutcome::Success => {
                        super::shell::ValidationExecutionOutcome::ExecutedSuccess
                    }
                    codex_tools::ToolOutputOutcome::Failure
                    | codex_tools::ToolOutputOutcome::TimedOut => {
                        super::shell::ValidationExecutionOutcome::ExecutedFailure
                    }
                    codex_tools::ToolOutputOutcome::Skipped => {
                        super::shell::ValidationExecutionOutcome::NotExecuted
                    }
                };
                (execution_outcome, output.code_mode_result(&payload))
            }
        }
        Err(error) => (
            super::shell::ValidationExecutionOutcome::ExecutedFailure,
            serde_json::json!({ "error": error.to_string() }),
        ),
    };
    Some(serde_json::json!({
        "step_id": candidate.step_id,
        "leaf_index": index,
        "call_id": synthetic_call_id,
        "freshness": "executed_or_shared",
        "success": execution_outcome.success(),
        "execution_outcome": execution_outcome.as_str(),
        "command_was_executed": execution_outcome != super::shell::ValidationExecutionOutcome::NotExecuted,
        "duration_ms": u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        "output": output,
    }))
}

impl CoreToolRuntime for PlanHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

fn parse_update_plan_arguments(arguments: &str) -> Result<PlanningUpdateInput, FunctionCallError> {
    let input = serde_json::from_str::<PlanningUpdateInput>(arguments).map_err(|e| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {e}"))
    })?;
    if input.tier == Some(PlanningTier::Focused) && !input.plan.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "focused planning uses a WorkUnitId and cannot create PlanStep records".to_string(),
        ));
    }
    if input.tier == Some(PlanningTier::Focused) && input.mutation_obligations.len() > 1 {
        return Err(FunctionCallError::RespondToModel(
            "focused planning accepts at most one atomic mutation obligation".to_string(),
        ));
    }
    for removal in input.removed_facts.iter().chain(&input.removed_steps) {
        if removal.id.trim().is_empty() || removal.reason.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "planning removals require a stable id and non-empty reason".to_string(),
            ));
        }
    }
    Ok(input)
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
