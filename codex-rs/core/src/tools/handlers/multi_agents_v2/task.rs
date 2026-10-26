use super::*;
use crate::agent::task_capabilities::ColdReviewContext;
use crate::agent::task_capabilities::ColdReviewContextInput;
use crate::agent::task_capabilities::RiskPolicyInput;
use crate::agent::task_capabilities::build_cold_review_context;
use crate::agent::task_capabilities::derive_risk_policy;
use codex_agent_task_store::AcceptanceCriterion;
use codex_agent_task_store::AgentGate;
use codex_agent_task_store::AgentReceipt;
use codex_agent_task_store::AgentRole;
use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::ArchitectureContractV1;
use codex_agent_task_store::AssignmentAdmissionOrigin;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptAmendment;
use codex_agent_task_store::AttemptId;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::DEFAULT_OBSERVATION_LIMIT;
use codex_agent_task_store::DeclaredChange;
use codex_agent_task_store::GateKind;
use codex_agent_task_store::GateStatus;
use codex_agent_task_store::MAX_OBSERVATION_LIMIT;
use codex_agent_task_store::ReceiptDraft;
use codex_agent_task_store::RelationKind;
use codex_agent_task_store::RepoScope;
use codex_agent_task_store::RiskDomain;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::TaskActor;
use codex_agent_task_store::ValidationCall;
use codex_git_utils::get_git_repo_root;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::validation::ValidationResult;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

const GET_AGENT_TASK_TOOL: &str = "get_agent_task";
const SUBMIT_AGENT_RECEIPT_TOOL: &str = "submit_agent_receipt";
const SET_AGENT_GATE_TOOL: &str = "set_agent_gate";
const AMEND_AGENT_TASK_TOOL: &str = "amend_agent_task";
const WAIVE_AGENT_GATE_TOOL: &str = "waive_agent_gate";
const ABANDON_AGENT_TASK_TOOL: &str = "abandon_agent_task";

pub(crate) struct GetAgentTaskHandler;
pub(crate) struct SubmitAgentReceiptHandler;
pub(crate) struct SetAgentGateHandler;
pub(crate) struct AmendAgentTaskHandler;
pub(crate) struct WaiveAgentGateHandler;
pub(crate) struct AbandonAgentTaskHandler;

macro_rules! define_handler {
    ($handler:ident, $tool_name:expr, $spec:ident, $handle:ident, $parallel:expr) => {
        impl ToolExecutor<ToolInvocation> for $handler {
            fn tool_name(&self) -> ToolName {
                ToolName::plain($tool_name)
            }

            fn spec(&self) -> ToolSpec {
                $spec()
            }

            fn supports_parallel_tool_calls(&self) -> bool {
                $parallel
            }

            fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
                Box::pin($handle(invocation))
            }
        }

        impl CoreToolRuntime for $handler {
            fn matches_kind(&self, payload: &ToolPayload) -> bool {
                matches!(payload, ToolPayload::Function { .. })
            }
        }
    };
}

define_handler!(
    GetAgentTaskHandler,
    GET_AGENT_TASK_TOOL,
    get_agent_task_spec,
    handle_get_agent_task,
    true
);
define_handler!(
    SubmitAgentReceiptHandler,
    SUBMIT_AGENT_RECEIPT_TOOL,
    submit_agent_receipt_spec,
    handle_submit_agent_receipt,
    false
);
define_handler!(
    SetAgentGateHandler,
    SET_AGENT_GATE_TOOL,
    set_agent_gate_spec,
    handle_set_agent_gate,
    false
);
define_handler!(
    AmendAgentTaskHandler,
    AMEND_AGENT_TASK_TOOL,
    amend_agent_task_spec,
    handle_amend_agent_task,
    false
);
define_handler!(
    WaiveAgentGateHandler,
    WAIVE_AGENT_GATE_TOOL,
    waive_agent_gate_spec,
    handle_waive_agent_gate,
    false
);
define_handler!(
    AbandonAgentTaskHandler,
    ABANDON_AGENT_TASK_TOOL,
    abandon_agent_task_spec,
    handle_abandon_agent_task,
    false
);

async fn handle_get_agent_task(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        call_id,
        ..
    } = invocation;
    let turn = Arc::clone(&step_context.turn);
    let arguments = function_arguments(payload)?;
    let args: GetAgentTaskArgs = parse_arguments(&arguments)?;
    let assignment_id = parse_assignment_id(GET_AGENT_TASK_TOOL, &args.assignment_id)?;
    let observation_limit = args
        .observation_limit
        .unwrap_or(DEFAULT_OBSERVATION_LIMIT)
        .min(MAX_OBSERVATION_LIMIT);

    let coordinator = session.services.agent_control.task_coordinator();
    let caller_binding = if turn.session_source.is_non_root_agent() {
        let binding = coordinator.binding_for_turn(&turn).ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "{GET_AGENT_TASK_TOOL}: non-root callers may only read their own current bound task"
            ))
        })?;
        Some(binding)
    } else {
        None
    };
    let task = coordinator
        .get_agent_task(assignment_id, Some(observation_limit))
        .await
        .map_err(|error| task_store_error(GET_AGENT_TASK_TOOL, error))?;
    if let Some(binding) = &caller_binding {
        let caller = if binding.assignment_id == assignment_id {
            task.clone()
        } else {
            coordinator
                .get_agent_task(binding.assignment_id, Some(0))
                .await
                .map_err(|error| task_store_error(GET_AGENT_TASK_TOOL, error))?
        };
        let current = caller.current_attempt.attempt_id == binding.attempt_id
            && caller.assignment.root_session_id == binding.root_session_id;
        let immediate_child = matches!(task.assignment.admission_origin,
            AssignmentAdmissionOrigin::LegacyMessage { parent_assignment_id: Some(parent) }
            if parent == binding.assignment_id)
            && caller.assignment.root_session_id == task.assignment.root_session_id
            && caller.assignment.repository_id == task.assignment.repository_id;
        if !current || (binding.assignment_id != assignment_id && !immediate_child) {
            return Err(FunctionCallError::RespondToModel(format!(
                "{GET_AGENT_TASK_TOOL}: non-root callers may read only their current bound task or its immediate children"
            )));
        }
    }
    let (cold_review_context, receipt_handoffs) = if caller_binding
        .as_ref()
        .is_some_and(|binding| binding.assignment_id == assignment_id)
    {
        build_evaluation_context(session.as_ref(), turn.config.cwd.as_path(), &task).await?
    } else {
        (None, Vec::new())
    };
    if let Some(binding) = &caller_binding
        && !receipt_handoffs.is_empty()
    {
        // Handoff acquisition can span several reads. Do not publish it for a
        // turn whose assignment was rebound to a correction in the meantime.
        let store = coordinator.store().ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "{GET_AGENT_TASK_TOOL}: the typed task store is unavailable"
            ))
        })?;
        let authorization = store
            .get_agent_task_authorization(binding.assignment_id)
            .await
            .map_err(|error| task_store_error(GET_AGENT_TASK_TOOL, error))?;
        if authorization.root_session_id != binding.root_session_id
            || authorization.current_attempt.attempt_id != binding.attempt_id
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "{GET_AGENT_TASK_TOOL}: receipt handoff requires the caller's current bound attempt"
            )));
        }
    }
    coordinator
        .emit_terminal_metrics_from_snapshot(&task, &turn.session_telemetry);
    let mut result = GetAgentTaskResult {
        task,
        cold_review_context,
        receipt_handoffs,
        handoff_recovery: None,
    };
    if !result.receipt_handoffs.is_empty() {
        let full = serde_json::to_value(&result).map_err(|error| {
            FunctionCallError::RespondToModel(format!("{GET_AGENT_TASK_TOOL}: cannot serialize evidence handoff: {error}"))
        })?;
        let canonical = codex_tools::CanonicalToolResult::json(full);
        // Optional evidence uses the ordinary snapshot owner. Requiring generic
        // canonical delivery would oblige Code Mode to read the entire handoff.
        let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
            &turn.config.codex_home, &session.thread_id.to_string(), &canonical,
        ).await;
        let artifact_id = artifact.artifact_id().filter(|_| {
            artifact.complete && artifact.retained_bytes == canonical.exact_bytes
                && artifact.unavailable_ranges.is_empty()
        }).ok_or_else(|| FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: complete evidence handoff could not be retained; retry this read"
        )))?;
        session.register_tool_artifact_origin(
            artifact_id.clone(), call_id, canonical.exact_bytes, canonical.sha256.clone(),
        ).await;
        result.handoff_recovery = Some(json!({
            "artifact_id": artifact_id,
            "canonical_sha256": canonical.sha256,
            "canonical_bytes": canonical.exact_bytes,
            "tool": "read_tool_output",
            "selector_kind": "json_pointer",
        }));
    }
    Ok(boxed_tool_output(result))
}

async fn handle_submit_agent_receipt(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        ..
    } = invocation;
    let turn = Arc::clone(&step_context.turn);
    let arguments = function_arguments(payload)?;
    let args: SubmitAgentReceiptArgs = parse_arguments(&arguments)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let binding = coordinator.binding_for_turn(&turn).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{SUBMIT_AGENT_RECEIPT_TOOL}: the caller is not a typed agent with a bound task"
        ))
    })?;
    // The binding proves both assignment and attempt ownership. Never retarget a stale worker
    // binding to a newer correction attempt: the root must first refresh the binding explicitly.
    let task = coordinator
        .get_agent_task(binding.assignment_id, Some(0))
        .await
        .map_err(|error| task_store_error(SUBMIT_AGENT_RECEIPT_TOOL, error))?;
    if binding.attempt_id != task.current_attempt.attempt_id {
        return Err(FunctionCallError::RespondToModel(format!(
            "{SUBMIT_AGENT_RECEIPT_TOOL}: the caller is bound to attempt {} but the current attempt is {}",
            binding.attempt_id, task.current_attempt.attempt_id
        )));
    }
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{SUBMIT_AGENT_RECEIPT_TOOL}: the typed task store is unavailable"
        ))
    })?;
    let draft = args.into_receipt_draft(&task)?;
    let receipt = store.submit_agent_receipt(binding.attempt_id, draft).await
        .map_err(|error| task_store_error(SUBMIT_AGENT_RECEIPT_TOOL, error))?;
    coordinator.mark_task_inactive(binding.assignment_id);
    coordinator
        .maybe_emit_terminal_metrics(binding.assignment_id, &turn.session_telemetry)
        .await;
    Ok(boxed_tool_output(SubmitAgentReceiptResult { receipt }))
}

#[derive(Default)]
struct ParsedRiskHints {
    high_risk_paths: Vec<RepoScope>,
    contracts: Vec<String>,
    domains: Vec<RiskDomain>,
}



async fn build_evaluation_context(
    session: &crate::session::session::Session,
    cwd: &Path,
    task: &AgentTask,
) -> Result<(Option<ColdReviewContext>, Vec<TaskReceiptHandoff>), FunctionCallError> {
    let (role_name, relation_kind) = match task.assignment.role {
        AgentRole::Reviewer => ("reviewer", RelationKind::Review),
        AgentRole::Verifier => ("verifier", RelationKind::Verification),
        AgentRole::Integrator => ("integrator", RelationKind::Integration),
        _ => return Ok((None, Vec::new())),
    };
    let relation = task.assignment.relation.as_ref().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: {role_name} assignment is missing its evaluation target"
        ))
    })?;
    if relation.kind != relation_kind
        || relation.target_assignment_ids.is_empty()
        || (task.assignment.role != AgentRole::Integrator
            && relation.target_assignment_ids.len() != 1)
        || relation.target_assignment_ids.iter().any(|target| {
            !task.assignment.dependencies.contains(target)
        })
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: {role_name} assignment has an invalid evaluation relation"
        )));
    }
    if task.assignment.role == AgentRole::Integrator {
        let mut handoffs = Vec::with_capacity(task.assignment.dependencies.len());
        for dependency in &task.assignment.dependencies {
            let target = load_receipt_handoff_target(session, task, *dependency).await?;
            handoffs.push(build_receipt_handoff(task, &target)?);
        }
        return Ok((None, handoffs));
    }
    let target = load_receipt_handoff_target(
        session,
        task,
        relation.target_assignment_ids[0],
    )
    .await?;
    let handoff = build_receipt_handoff(task, &target)?;
    let target_receipt_scope = crate::agent::task_capabilities::cold_review_receipt_scope(&target)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{GET_AGENT_TASK_TOOL}: target receipt scope is invalid: {error}"
            ))
        })?;
    let agents_md_observation = session
        .services
        .agents_md_manager
        .get_cached_observation()
        .await;
    let agents_md_freshness = agents_md_observation.freshness.model_visible_description();
    let applicable_instructions = agents_md_observation
        .loaded
        .and_then(|instructions| {
            let text = instructions.text();
            (!text.trim().is_empty()).then(|| format!("{agents_md_freshness}\n\n{text}"))
        })
        .into_iter()
        .collect();
    let mut relevant_contracts = parse_risk_hints(&target.assignment.risk_hints).contracts;
    if let Some(contract) = target
        .receipt
        .as_ref()
        .and_then(|receipt| receipt.architecture_contract.as_ref())
    {
        relevant_contracts.push(serde_json::to_string(contract).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{GET_AGENT_TASK_TOOL}: invalid sealed architecture contract: {error}"
            ))
        })?);
    }
    let nearest_tests = target
        .assignment
        .required_evidence
        .iter()
        .cloned()
        .chain(
            target
                .validation_calls
                .iter()
                .map(|call| call.command_summary.clone()),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
        build_cold_review_context(
            &repo_root,
            ColdReviewContextInput {
                assignment: target.assignment,
                amendment: target.current_attempt.amendment,
                attempt_id: target.current_attempt.attempt_id,
                applicable_instructions,
                attempt_specific_diff: "Automatic change snapshots are disabled. Inspect the declared changes independently; task receipts do not establish current workspace freshness.".to_string(),
                observed_writes: Vec::new(),
                relevant_contracts,
                nearest_tests,
            },
        )
    })
    .await
    .map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: cold-review evidence worker failed: {error}"
        ))
    })?
    .map(|mut context| {
        context.target_receipt_scope = target_receipt_scope;
        (Some(context), vec![handoff])
    })
    .map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: cold-review evidence is invalid: {error}"
        ))
    })
}

async fn load_receipt_handoff_target(
    session: &crate::session::session::Session,
    task: &AgentTask,
    target_assignment_id: AssignmentId,
) -> Result<AgentTask, FunctionCallError> {
    // The caller cannot supply a target ID: only the current bound assignment's
    // admitted dependencies authorize this closed handoff, never a sibling read.
    if !task.assignment.dependencies.contains(&target_assignment_id) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: receipt handoff target is not an assigned dependency"
        )));
    }
    let target = session
        .services
        .agent_control
        .task_coordinator()
        .get_agent_task(target_assignment_id, Some(0))
        .await
        .map_err(|error| task_store_error(GET_AGENT_TASK_TOOL, error))?;
    if target.assignment.root_session_id != task.assignment.root_session_id
        || target.assignment.repository_id != task.assignment.repository_id
        || target.current_attempt.assignment_id != target_assignment_id
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: receipt handoff target is outside the bound task's root or repository"
        )));
    }
    Ok(target)
}

fn build_receipt_handoff(
    task: &AgentTask,
    target: &AgentTask,
) -> Result<TaskReceiptHandoff, FunctionCallError> {
    let mut evidence_call_ids = BTreeSet::new();
    if let Some(receipt) = &target.receipt {
        if receipt.assignment_id != target.assignment.assignment_id
            || receipt.attempt_id != target.current_attempt.attempt_id
            || !target.current_attempt.state.is_terminal()
            || target.current_attempt.sealed_at != Some(receipt.sealed_at)
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "{GET_AGENT_TASK_TOOL}: receipt handoff does not match the target's current sealed attempt"
            )));
        }
        evidence_call_ids.extend(receipt.validation_call_ids.iter().cloned());
        evidence_call_ids.extend(receipt.criterion_results.iter().filter_map(|result| {
            result.evidence_ref.as_ref().map(|reference| reference.call_id.clone())
        }));
    }
    let validation_calls = target.validation_calls.iter()
        .filter(|call| evidence_call_ids.contains(&call.call_id))
        .cloned()
        .collect::<Vec<_>>();
    if validation_calls.iter().any(|call| call.attempt_id != target.current_attempt.attempt_id) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: receipt evidence belongs to another target attempt"
        )));
    }
    let unavailable_evidence_call_ids = evidence_call_ids.into_iter()
        .filter(|id| !validation_calls.iter().any(|call| &call.call_id == id))
        .collect();
    let admitted_dependency = task.dependency_admissions.iter()
        .find(|admission| admission.assignment_id == target.assignment.assignment_id)
        .cloned();
    Ok(TaskReceiptHandoff {
        assignment_id: target.assignment.assignment_id,
        role: target.assignment.role,
        workspace_id: target.assignment.workspace_id.clone(),
        current_attempt: target.current_attempt.clone(),
        effective_contract: target.assignment.effective_contract(target.current_attempt.amendment.as_ref()),
        admitted_attempt_is_current: admitted_dependency.as_ref()
            .map(|admission| admission.attempt_id == target.current_attempt.attempt_id),
        admitted_dependency,
        receipt: target.receipt.clone(),
        dependency_blockers: target.dependency_blockers.clone(),
        validation_calls,
        unavailable_evidence_call_ids,
        gates: target.gates.clone(),
        evidence_scope: "Current-attempt sealed receipt and its referenced captured evidence at this read, not an admission-time snapshot or proof of current workspace freshness. Receipt prose and declared changes are worker reports, not independently verified facts. No receipt means evidence is unavailable, not an empty result.",
    })
}

fn parse_risk_hints(hints: &[String]) -> ParsedRiskHints {
    let mut paths = BTreeSet::new();
    let mut contracts = BTreeSet::new();
    let mut domains = BTreeSet::new();
    for hint in hints {
        let hint = hint.trim();
        if hint.is_empty() {
            continue;
        }
        if let Some((kind, value)) = hint.split_once(':') {
            let value = value.trim();
            if kind.trim().eq_ignore_ascii_case("path") && !value.is_empty() {
                paths.insert(value.to_string());
                continue;
            }
            if kind.trim().eq_ignore_ascii_case("contract") && !value.is_empty() {
                contracts.insert(value.to_string());
                continue;
            }
        }
        if let Some(domain) = risk_domain_from_hint(hint) {
            domains.insert(domain);
        } else {
            // Unstructured root-authored hints are conservatively treated as contract risks.
            contracts.insert(hint.to_string());
        }
    }
    ParsedRiskHints {
        high_risk_paths: paths
            .into_iter()
            .map(|path| RepoScope {
                path,
                recursive: true,
            })
            .collect(),
        contracts: contracts.into_iter().collect(),
        domains: domains.into_iter().collect(),
    }
}

fn risk_domain_from_hint(hint: &str) -> Option<RiskDomain> {
    let normalized = hint.trim().to_ascii_lowercase().replace(['-', '_'], " ");
    let normalized = normalized.strip_suffix(" risk").unwrap_or(&normalized);
    match normalized {
        "concurrency" => Some(RiskDomain::Concurrency),
        "unsafe" | "unsafe code" => Some(RiskDomain::UnsafeCode),
        "lifecycle" => Some(RiskDomain::Lifecycle),
        "persistence" => Some(RiskDomain::Persistence),
        "schema" => Some(RiskDomain::Schema),
        "protocol" => Some(RiskDomain::Protocol),
        "security" => Some(RiskDomain::Security),
        "installation" => Some(RiskDomain::Installation),
        _ => None,
    }
}

async fn handle_set_agent_gate(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        ..
    } = invocation;
    let turn = Arc::clone(&step_context.turn);
    let arguments = function_arguments(payload)?;
    let args: SetAgentGateArgs = parse_arguments(&arguments)?;
    let assignment_id = parse_assignment_id(SET_AGENT_GATE_TOOL, &args.assignment_id)?;
    let expected_attempt_id = args
        .expected_attempt_id
        .parse::<AttemptId>()
        .map_err(|error| task_store_error(SET_AGENT_GATE_TOOL, error))?;
    let coordinator = session.services.agent_control.task_coordinator();
    let actor = if turn.session_source.is_non_root_agent() {
        let binding = coordinator.binding_for_turn(&turn).ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "{SET_AGENT_GATE_TOOL}: the caller is not a typed agent with a bound task"
            ))
        })?;
        TaskActor::Attempt(binding.attempt_id)
    } else {
        coordinator
            .root_actor()
            .map_err(|error| task_store_error(SET_AGENT_GATE_TOOL, error))?
    };
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{SET_AGENT_GATE_TOOL}: the typed task store is unavailable"
        ))
    })?;
    let gate = store
        .set_agent_gate(
            actor,
            assignment_id,
            expected_attempt_id,
            args.gate,
            args.status,
            args.reason,
        )
        .await
        .map_err(|error| task_store_error(SET_AGENT_GATE_TOOL, error))?;
    coordinator
        .maybe_emit_terminal_metrics(assignment_id, &turn.session_telemetry)
        .await;
    Ok(boxed_tool_output(SetAgentGateResult { gate }))
}

async fn handle_amend_agent_task(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        ..
    } = invocation;
    let turn = Arc::clone(&step_context.turn);
    let arguments = function_arguments(payload)?;
    let args: AmendAgentTaskArgs = parse_arguments(&arguments)?;
    require_root(&turn.session_source, AMEND_AGENT_TASK_TOOL)?;
    let assignment_id = parse_assignment_id(AMEND_AGENT_TASK_TOOL, &args.assignment_id)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let attempt = coordinator
        .amend_agent_task(assignment_id, args.into_amendment())
        .await
        .map_err(|error| task_store_error(AMEND_AGENT_TASK_TOOL, error))?;

    Ok(boxed_tool_output(AmendAgentTaskResult { attempt }))
}

async fn handle_waive_agent_gate(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        ..
    } = invocation;
    let turn = Arc::clone(&step_context.turn);
    let arguments = function_arguments(payload)?;
    let args: WaiveAgentGateArgs = parse_arguments(&arguments)?;
    require_root(&turn.session_source, WAIVE_AGENT_GATE_TOOL)?;
    let assignment_id = parse_assignment_id(WAIVE_AGENT_GATE_TOOL, &args.assignment_id)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{WAIVE_AGENT_GATE_TOOL}: the typed task store is unavailable"
        ))
    })?;
    let gate = store
        .waive_agent_gate(
            coordinator
                .root_actor()
                .map_err(|error| task_store_error(WAIVE_AGENT_GATE_TOOL, error))?,
            assignment_id,
            args.gate.into(),
            args.reason,
        )
        .await
        .map_err(|error| task_store_error(WAIVE_AGENT_GATE_TOOL, error))?;
    coordinator
        .maybe_emit_terminal_metrics(assignment_id, &turn.session_telemetry)
        .await;
    Ok(boxed_tool_output(WaiveAgentGateResult { gate }))
}

async fn handle_abandon_agent_task(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        ..
    } = invocation;
    let turn = Arc::clone(&step_context.turn);
    let arguments = function_arguments(payload)?;
    let args: AbandonAgentTaskArgs = parse_arguments(&arguments)?;
    require_root(&turn.session_source, ABANDON_AGENT_TASK_TOOL)?;
    let assignment_id = parse_assignment_id(ABANDON_AGENT_TASK_TOOL, &args.assignment_id)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{ABANDON_AGENT_TASK_TOOL}: the typed task store is unavailable"
        ))
    })?;
    let receipt = store
        .abandon_agent_task(
            coordinator
                .root_actor()
                .map_err(|error| task_store_error(ABANDON_AGENT_TASK_TOOL, error))?,
            assignment_id,
            args.reason,
        )
        .await
        .map_err(|error| task_store_error(ABANDON_AGENT_TASK_TOOL, error))?;
    coordinator.mark_task_inactive(assignment_id);
    coordinator
        .maybe_emit_terminal_metrics(assignment_id, &turn.session_telemetry)
        .await;

    // Durable abandonment is authoritative. Every following step is best-effort so a missing or
    // already-dead runtime thread cannot turn a sealed result into a reported tool failure.
    let binding = match coordinator.binding_for_assignment(assignment_id) {
        Some(binding) => Some(binding),
        None => coordinator
            .get_agent_task_binding(assignment_id)
            .await
            .ok()
            .flatten(),
    };
    if let Some(binding) = binding {
        let thread_id = binding
            .thread_id
            .as_deref()
            .and_then(|thread_id| ThreadId::from_string(thread_id).ok());
        let thread_id = match thread_id {
            Some(thread_id) => Some(thread_id),
            None => session
                .services
                .agent_control
                .resolve_agent_reference(
                    session.thread_id,
                    &turn.session_source,
                    &binding.agent_path,
                )
                .await
                .ok(),
        };
        if let Some(thread_id) = thread_id {
            let _ = session
                .services
                .agent_control
                .interrupt_agent(thread_id)
                .await;
        }
    }
    Ok(boxed_tool_output(AbandonAgentTaskResult { receipt }))
}

fn parse_assignment_id(
    tool_name: &'static str,
    value: &str,
) -> Result<AssignmentId, FunctionCallError> {
    AssignmentId::parse(value).map_err(|error| task_store_error(tool_name, error))
}

fn require_root(
    session_source: &SessionSource,
    tool_name: &'static str,
) -> Result<(), FunctionCallError> {
    if session_source.is_non_root_agent() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name}: this operation is root-only"
        )));
    }
    Ok(())
}

fn task_store_error(tool_name: &'static str, error: StoreError) -> FunctionCallError {
    let detail = task_store_error_detail(tool_name, error);
    FunctionCallError::RespondToModel(format!("{tool_name}: {detail}"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetAgentTaskArgs {
    assignment_id: String,
    observation_limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitAgentReceiptArgs {
    status: AgentStatusClaim,
    summary: String,
    criterion_results: Vec<ReceiptCriterionArgs>,
    declared_changes: Vec<DeclaredChangeArgs>,
    validation_call_ids: Option<Vec<String>>,
    #[serde(default)]
    blockers: Vec<String>,
    #[serde(default)]
    risks: Vec<String>,
    next_action: Option<String>,
    #[serde(default)]
    architecture_contract: Option<ArchitectureContractV1>,
}

impl SubmitAgentReceiptArgs {
    fn into_receipt_draft(self, task: &AgentTask) -> Result<ReceiptDraft, FunctionCallError> {
        let criterion_results = self
            .criterion_results
            .into_iter()
            .map(|criterion| criterion.into_criterion_result(task))
            .collect::<Result<Vec<_>, _>>()?;
        // Infer only cited calls, not every call the attempt executed. Explicit lists retain
        // their meaning and the store still validates ownership, success, epoch, and coverage.
        let validation_call_ids = self.validation_call_ids.unwrap_or_else(|| {
            criterion_results
                .iter()
                .filter_map(|criterion| criterion.evidence_ref.as_ref())
                .map(|reference| reference.call_id.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        });
        Ok(ReceiptDraft {
            status: self.status,
            summary: self.summary,
            criterion_results,
            declared_changes: self
                .declared_changes
                .into_iter()
                .map(DeclaredChangeArgs::into_declared_change)
                .collect(),
            validation_call_ids,
            blockers: self.blockers,
            risks: self.risks,
            next_action: self.next_action,
            architecture_contract: self.architecture_contract,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptCriterionArgs {
    criterion_id: String,
    status: CriterionStatus,
    evidence: Option<String>,
    evidence_ref: Option<ReceiptEvidenceRefArgs>,
}

impl ReceiptCriterionArgs {
    fn into_criterion_result(self, task: &AgentTask) -> Result<CriterionResult, FunctionCallError> {
        Ok(CriterionResult {
            evidence_ref: self
                .evidence_ref
                .map(|reference| reference.resolve(task))
                .transpose()?,
            criterion_id: self.criterion_id,
            status: self.status,
            evidence: self.evidence,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptEvidenceRefArgs {
    call_id: String,
    workspace_id: Option<String>,
    evidence_epoch: Option<u64>,
    kind: Option<codex_agent_task_store::CriterionEvidenceKind>,
}

impl ReceiptEvidenceRefArgs {
    fn resolve(
        self,
        task: &AgentTask,
    ) -> Result<codex_agent_task_store::CriterionEvidenceRef, FunctionCallError> {
        let evidence_epoch = match self.evidence_epoch {
            Some(epoch) => epoch,
            None => task.validation_calls.iter()
                .find(|call| call.call_id == self.call_id && call.attempt_id == task.current_attempt.attempt_id)
                .and_then(|call| call.evidence.end_epoch)
                .ok_or_else(|| FunctionCallError::RespondToModel(format!(
                    "{SUBMIT_AGENT_RECEIPT_TOOL}: validation call {} has no recorded end epoch in the bound attempt",
                    self.call_id
                )))?,
        };
        let kind = self
            .kind
            .or_else(|| {
                task.validation_calls
                    .iter()
                    .find(|call| call.call_id == self.call_id)
                    .and_then(ValidationCall::verified_evidence_kind)
            })
            .unwrap_or(codex_agent_task_store::CriterionEvidenceKind::ValidationExecution);
        Ok(codex_agent_task_store::CriterionEvidenceRef {
            call_id: self.call_id,
            workspace_id: self
                .workspace_id
                .unwrap_or_else(|| task.assignment.workspace_id.clone()),
            evidence_epoch,
            kind,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredChangeArgs {
    path: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetAgentGateArgs {
    assignment_id: String,
    expected_attempt_id: String,
    gate: GateKind,
    status: GateStatus,
    reason: String,
}

impl DeclaredChangeArgs {
    fn into_declared_change(self) -> DeclaredChange {
        DeclaredChange {
            path: self.path,
            summary: self.summary,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AmendAgentTaskArgs {
    assignment_id: String,
    reason: String,
    objective: Option<String>,
    acceptance_criteria: Option<Vec<AcceptanceCriterionArgs>>,
    stop_condition: Option<String>,
}

impl AmendAgentTaskArgs {
    fn into_amendment(self) -> AttemptAmendment {
        AttemptAmendment {
            reason: self.reason,
            objective: self.objective,
            acceptance_criteria: self.acceptance_criteria.map(|criteria| {
                criteria
                    .into_iter()
                    .map(AcceptanceCriterionArgs::into_acceptance_criterion)
                    .collect()
            }),
            stop_condition: self.stop_condition,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceCriterionArgs {
    id: String,
    text: String,
    #[serde(default)]
    evidence_policy: Option<codex_agent_task_store::CriterionEvidencePolicy>,
}

impl AcceptanceCriterionArgs {
    fn into_acceptance_criterion(self) -> AcceptanceCriterion {
        AcceptanceCriterion {
            evidence_policy: self.evidence_policy,
            id: self.id,
            text: self.text,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WaivableGateKind {
    Review,
    Verification,
}

impl From<WaivableGateKind> for GateKind {
    fn from(value: WaivableGateKind) -> Self {
        match value {
            WaivableGateKind::Review => Self::Review,
            WaivableGateKind::Verification => Self::Verification,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaiveAgentGateArgs {
    assignment_id: String,
    gate: WaivableGateKind,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AbandonAgentTaskArgs {
    assignment_id: String,
    reason: String,
}

#[derive(Debug)]
struct GetAgentTaskResult {
    task: AgentTask,
    cold_review_context: Option<ColdReviewContext>,
    receipt_handoffs: Vec<TaskReceiptHandoff>,
    handoff_recovery: Option<JsonValue>,
}

/// Closed task-to-task evidence transfer: no observations, tool history, or transcript.
#[derive(Debug, Serialize)]
struct TaskReceiptHandoff {
    assignment_id: AssignmentId,
    role: AgentRole,
    workspace_id: String,
    current_attempt: Attempt,
    effective_contract: codex_agent_task_store::EffectiveTaskContract,
    admitted_dependency: Option<codex_agent_task_store::DependencyAdmission>,
    admitted_attempt_is_current: Option<bool>,
    receipt: Option<AgentReceipt>,
    dependency_blockers: Vec<codex_agent_task_store::DependencyBlocker>,
    validation_calls: Vec<ValidationCall>,
    unavailable_evidence_call_ids: Vec<String>,
    gates: Vec<AgentGate>,
    evidence_scope: &'static str,
}

#[derive(Serialize)]
struct ColdReviewContextWithReceipt<'a> {
    #[serde(flatten)]
    context: &'a ColdReviewContext,
    target_receipt_handoff: Option<&'a TaskReceiptHandoff>,
}

impl Serialize for GetAgentTaskResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct(
            "GetAgentTaskResult",
            1 + usize::from(self.cold_review_context.is_some())
                + usize::from(self.task.assignment.role == AgentRole::Integrator
                    && !self.receipt_handoffs.is_empty()),
        )?;
        value.serialize_field("task", &self.task)?;
        if let Some(context) = &self.cold_review_context {
            value.serialize_field("cold_review_context", &ColdReviewContextWithReceipt {
                context,
                target_receipt_handoff: self.receipt_handoffs.first(),
            })?;
        }
        if self.task.assignment.role == AgentRole::Integrator && !self.receipt_handoffs.is_empty() {
            value.serialize_field("dependency_receipt_handoffs", &self.receipt_handoffs)?;
        }
        value.end()
    }
}

#[derive(Debug)]
struct SubmitAgentReceiptResult {
    receipt: AgentReceipt,
}

impl Serialize for SubmitAgentReceiptResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("SubmitAgentReceiptResult", 2)?;
        value.serialize_field("receipt", &self.receipt)?;
        value.serialize_field(
            "completion_evidence",
            &bounded_receipt_projection(&self.receipt),
        )?;
        value.end()
    }
}

const TASK_OUTPUT_INLINE_LIMIT_BYTES: usize = 8 * 1024;
const MAX_PROJECTED_CHANGED_PATHS: usize = 8;
const MAX_PROJECTED_PROOF_REFERENCES: usize = 8;
const MAX_PROJECTED_VALIDATION_CALLS: usize = 8;
const MAX_PROJECTED_RECENT_OBSERVATIONS: usize = 4;
const MAX_PROJECTED_OBLIGATIONS_PER_KIND: usize = 4;
const MAX_PROJECTED_EXACT_STRING_BYTES: usize = 192;
const MAX_PROJECTED_PROSE_BYTES: usize = 512;
const MAX_PROJECTED_DETAIL_BYTES: usize = 384;
const MAX_PREDETERMINED_JSON_POINTERS: usize = 64;

fn bounded_receipt_projection(receipt: &AgentReceipt) -> JsonValue {
    bounded_receipt_projection_with_proof_limit(receipt, MAX_PROJECTED_PROOF_REFERENCES)
}

fn bounded_receipt_projection_with_proof_limit(
    receipt: &AgentReceipt,
    proof_limit: usize,
) -> JsonValue {
    let (changed_paths, omitted_changed_paths) = bounded_exact_strings(
        receipt
            .declared_changes
            .iter()
            .map(|change| change.path.as_str()),
        MAX_PROJECTED_CHANGED_PATHS,
    );
    let (proof_references, omitted_proof_references) = bounded_exact_strings(
        receipt.validation_call_ids.iter().map(String::as_str),
        proof_limit,
    );
    let passed = receipt
        .criterion_results
        .iter()
        .filter(|result| result.status == CriterionStatus::Passed)
        .count();
    let failed = receipt
        .criterion_results
        .iter()
        .filter(|result| result.status == CriterionStatus::Failed)
        .count();
    json!({
        "assignment_id": receipt.assignment_id,
        "attempt_id": receipt.attempt_id,
        "status": receipt.status,
        "evidence_epoch": receipt.evidence_epoch,
        "sealed_at": receipt.sealed_at,
        "changed_paths": changed_paths,
        "criterion_counts": {
            "total": receipt.criterion_results.len(),
            "passed": passed,
            "reported_passed": passed,
            "with_evidence_reference": receipt.criterion_results.iter().filter(|result| result.evidence_ref.is_some()).count(),
            "with_execution_reference": receipt.criterion_results.iter().filter(|result| result.evidence_ref.as_ref().is_some_and(|reference| reference.kind == codex_agent_task_store::CriterionEvidenceKind::ValidationExecution)).count(),
            "with_source_inspection_reference": receipt.criterion_results.iter().filter(|result| result.evidence_ref.as_ref().is_some_and(|reference| reference.kind == codex_agent_task_store::CriterionEvidenceKind::SourceInspection)).count(),
            "reported_passed_without_execution_reference": receipt.criterion_results.iter().filter(|result| result.status == CriterionStatus::Passed && !result.evidence_ref.as_ref().is_some_and(|reference| reference.kind == codex_agent_task_store::CriterionEvidenceKind::ValidationExecution)).count(),
            "reported_passed_without_evidence_reference": receipt.criterion_results.iter().filter(|result| result.status == CriterionStatus::Passed && result.evidence_ref.is_none()).count(),
            "failed": failed,
            "not_run": receipt.criterion_results.len().saturating_sub(passed + failed),
        },
        "criterion_execution_references": receipt.criterion_results.iter().filter_map(|result| {
            result.evidence_ref.as_ref().filter(|reference| reference.kind == codex_agent_task_store::CriterionEvidenceKind::ValidationExecution && result.criterion_id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES && reference.call_id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES && reference.workspace_id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES)
                .map(|reference| json!({"criterion_id": result.criterion_id, "execution": reference}))
        }).take(proof_limit).collect::<Vec<_>>(),
        "criterion_source_inspection_references": receipt.criterion_results.iter().filter_map(|result| {
            result.evidence_ref.as_ref().filter(|reference| reference.kind == codex_agent_task_store::CriterionEvidenceKind::SourceInspection && result.criterion_id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES && reference.call_id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES && reference.workspace_id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES)
                .map(|reference| json!({"criterion_id": result.criterion_id, "source_inspection": reference}))
        }).take(proof_limit).collect::<Vec<_>>(),
        "evidence_scope": "Recorded source acquisitions and validation executions, distinguished by producer kind. Source acquisition is not execution or semantic review; test coverage and current freshness are not established by this receipt alone.",
        "omitted_item_counts": {
            "changed_paths": omitted_changed_paths,
            "proof_references": omitted_proof_references,
            "blockers": receipt.blockers.len(),
            "risks": receipt.risks.len(),
            "criterion_evidence": receipt.criterion_results.len(),
        },
    })
}

fn bounded_exact_strings<'a>(
    values: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> (Vec<String>, usize) {
    let values = values.into_iter().collect::<Vec<_>>();
    let retained = values
        .iter()
        .copied()
        .filter(|value| value.len() <= MAX_PROJECTED_EXACT_STRING_BYTES)
        .take(limit)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let omitted = values.len().saturating_sub(retained.len());
    (retained, omitted)
}

fn bounded_prose(value: Option<&str>) -> Option<String> {
    bounded_prose_to(value, MAX_PROJECTED_PROSE_BYTES)
}

fn bounded_detail(value: Option<&str>) -> Option<String> {
    bounded_prose_to(value, MAX_PROJECTED_DETAIL_BYTES)
}

fn bounded_prose_to(value: Option<&str>, limit: usize) -> Option<String> {
    let value = value?;
    if value.len() <= limit {
        return Some(value.to_string());
    }
    let mut end = limit.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    Some(format!("{}...", &value[..end]))
}


fn task_projection(result: &GetAgentTaskResult) -> JsonValue {
    let task = &result.task;
    let recent_observation_start = task
        .observations
        .len()
        .saturating_sub(MAX_PROJECTED_RECENT_OBSERVATIONS);
    let recent_observations = task.observations[recent_observation_start..]
        .iter()
        .map(|observation| {
            json!({
                "event_id": observation.event_id,
                "wake_event_id": observation.wake_event_id,
                "attempt_id": observation.attempt_id,
                "execution_stop_cause": observation.execution_stop_cause,
                "kind": observation.kind,
                "summary": bounded_detail(Some(&observation.summary)),
                "call_id": observation.call_id.as_deref().filter(|value| value.len() <= MAX_PROJECTED_EXACT_STRING_BYTES),
                "created_at": observation.created_at,
            })
        })
        .collect::<Vec<_>>();
    let criteria = task
        .current_attempt
        .amendment
        .as_ref()
        .and_then(|amendment| amendment.acceptance_criteria.as_deref())
        .unwrap_or(&task.assignment.acceptance_criteria);
    let unresolved_criteria = criteria
        .iter()
        .filter_map(|criterion| {
            let status = task.receipt.as_ref().and_then(|receipt| {
                receipt
                    .criterion_results
                    .iter()
                    .find(|result| result.criterion_id == criterion.id)
                    .map(|result| result.status)
            });
            if task.criterion_is_satisfied(&criterion.id)
                || criterion.id.len() > MAX_PROJECTED_EXACT_STRING_BYTES
            {
                return None;
            }
            Some(json!({
                "id": criterion.id,
                "status": status,
                "text": bounded_detail(Some(&criterion.text)),
            }))
        })
        .take(MAX_PROJECTED_OBLIGATIONS_PER_KIND)
        .collect::<Vec<_>>();
    let unresolved_criteria_total = criteria
        .iter()
        .filter(|criterion| !task.criterion_is_satisfied(&criterion.id))
        .count();
    let omitted_observations = task
        .observations
        .len()
        .saturating_sub(recent_observations.len());
    let omitted_acceptance_criteria = criteria.len().saturating_sub(unresolved_criteria.len());
    let omitted_unresolved_criteria =
        unresolved_criteria_total.saturating_sub(unresolved_criteria.len());
    let target_scope = result.cold_review_context.as_ref().map(|context| {
        context.target_receipt_scope.as_ref().map(|scope| {
            let paths = scope.declared_paths.iter()
                .filter(|path| path.len() <= MAX_PROJECTED_EXACT_STRING_BYTES)
                .take(MAX_PROJECTED_CHANGED_PATHS).collect::<Vec<_>>();
            json!({
                "assignment_id": scope.assignment_id, "attempt_id": scope.attempt_id,
                "status": scope.status, "provenance": scope.provenance, "freshness": scope.freshness,
                "declared_paths": paths, "total_paths": scope.declared_paths.len(),
                "delivered_paths": paths.len(), "omitted_paths": scope.declared_paths.len() - paths.len(),
                "recovery_json_pointer": "/cold_review_context/target_receipt_scope",
            })
        })
    });
    let handoffs = result.receipt_handoffs.iter().enumerate()
        .take(MAX_PROJECTED_OBLIGATIONS_PER_KIND)
        .map(|(index, handoff)| {
            let pointer = if result.cold_review_context.is_some() {
                "/cold_review_context/target_receipt_handoff".to_string()
            } else {
                format!("/dependency_receipt_handoffs/{index}")
            };
            json!({
                "assignment_id": handoff.assignment_id,
                "attempt_id": handoff.current_attempt.attempt_id,
                "attempt_state": handoff.current_attempt.state,
                "admitted_attempt_is_current": handoff.admitted_attempt_is_current,
                "dependency_blocker_count": handoff.dependency_blockers.len(),
                "receipt": handoff.receipt.as_ref().map(|receipt| json!({
                    "status": receipt.status,
                    "sealed_at": receipt.sealed_at,
                    "summary": bounded_detail(Some(&receipt.summary)),
                    "criterion_count": receipt.criterion_results.len(),
                    "changed_path_count": receipt.declared_changes.len(),
                    "blocker_count": receipt.blockers.len(),
                    "risk_count": receipt.risks.len(),
                })),
                "captured_evidence_count": handoff.validation_calls.len(),
                "unavailable_evidence_count": handoff.unavailable_evidence_call_ids.len(),
                "contract_recovery_json_pointer": format!("{pointer}/effective_contract"),
                "receipt_recovery_json_pointer": handoff.receipt.as_ref().map(|_| format!("{pointer}/receipt")),
                "evidence_recovery_json_pointer": format!("{pointer}/validation_calls"),
                "recovery_json_pointer": pointer,
            })
        }).collect::<Vec<_>>();
    json!({
        "canonical_recovery": result.handoff_recovery,
        "target_receipt_scope": target_scope,
        "receipt_handoffs": {
            "count": result.receipt_handoffs.len(),
            "policy": "Assigned target/dependency summaries, not verified proof. A null receipt means unavailable. Recover only the contract, receipt fields or captured evidence needed for the decision with read_tool_output JSON pointers. No direct target/sibling access; workspace freshness is unverified.",
            "summaries": handoffs,
            "omitted_summaries": result.receipt_handoffs.len().saturating_sub(handoffs.len()),
            "context_recovery_json_pointers": result.cold_review_context.as_ref().map(|_| [
                "/cold_review_context/effective_contract",
                "/cold_review_context/applicable_instructions",
                "/cold_review_context/attempt_specific_diff",
                "/cold_review_context/observed_writes",
                "/cold_review_context/relevant_contracts",
                "/cold_review_context/nearest_tests",
            ]),
            "recovery_json_pointer": if result.cold_review_context.is_some() {
                Some("/cold_review_context/target_receipt_handoff")
            } else if task.assignment.role == AgentRole::Integrator && !result.receipt_handoffs.is_empty() {
                Some("/dependency_receipt_handoffs")
            } else {
                None
            },
        },
        "assignment_id": task.assignment.assignment_id,
        "attempt_id": task.current_attempt.attempt_id,
        "contract_recovery_json_pointers": ["/task/assignment", "/task/current_attempt/amendment"],
        "role": task.assignment.role,
        "attempt_state": task.current_attempt.state,
        "dependency_provenance": {
            "policy": "pin_at_admission; current durable authority checked separately; not a filesystem-freshness claim",
            "captured": task.dependency_admissions.len(),
            "recovery_json_pointer": "/task/dependency_admissions",
            "current_blockers": task.dependency_blockers.len(),
            "blockers_recovery_json_pointer": "/task/dependency_blockers",
        },
        "gates": task.gates.iter().map(|gate| json!({
            "kind": gate.kind,
            "status": gate.status,
        })).collect::<Vec<_>>(),
        "unresolved_gates": task.gates.iter().filter(|gate| !matches!(gate.status, GateStatus::Passed | GateStatus::Waived)).map(|gate| json!({
            "kind": gate.kind,
            "status": gate.status,
            "reason": bounded_detail(Some(&gate.reason)),
            "evidence_epoch": gate.evidence_epoch,
        })).collect::<Vec<_>>(),
        "unresolved_obligations": {
            "acceptance_criteria": unresolved_criteria,
        },
        "criterion_assessments": {
            "scope": "Explicit evidence policies only; source-acquisition or historical exact-command execution applicability, not semantic correctness or current execution freshness.",
            "satisfied": task.criterion_assessments.iter().filter(|assessment| assessment.satisfied).count(),
            "unsatisfied": task.criterion_assessments.iter().filter(|assessment| !assessment.satisfied).count(),
            "recovery_json_pointer": "/task/criterion_assessments",
        },
        "recent_observations": recent_observations,
        "receipt": task.receipt.as_ref().map(|receipt| bounded_receipt_projection_with_proof_limit(receipt, 0)),
        "workspace": {
            "last_progress_at": task.workspace_status.last_progress_at,
            "lease_state": task.workspace_status.lease_state,
            "next_required_action": bounded_prose(task.workspace_status.next_required_action.as_deref()),
        },
        "omitted_item_counts": {
            "observations": omitted_observations,
            "acceptance_criteria": omitted_acceptance_criteria,
            "unresolved_acceptance_criteria": omitted_unresolved_criteria,
            "required_evidence": task.assignment.required_evidence.len(),
            "read_scope": task.assignment.read_scope.len(),
            "write_scope": task.assignment.write_scope.len(),
        },
    })
}

fn push_predetermined_json_pointer(
    selectors: &mut Vec<codex_tools::ToolOutputProjectionJsonPointer>,
    id: impl Into<String>,
    pointer: impl Into<String>,
) {
    if selectors.len() >= MAX_PREDETERMINED_JSON_POINTERS {
        return;
    }
    let id = id.into();
    let pointer = pointer.into();
    if selectors
        .iter()
        .any(|selector| selector.id == id || selector.pointer == pointer)
    {
        return;
    }
    selectors.push(codex_tools::ToolOutputProjectionJsonPointer { id, pointer });
}

fn append_receipt_projection_json_pointers(
    selectors: &mut Vec<codex_tools::ToolOutputProjectionJsonPointer>,
    receipt: &AgentReceipt,
    pointer_prefix: &str,
    id_prefix: &str,
    proof_limit: usize,
) {
    for (index, _) in receipt.blockers.iter().enumerate() {
        push_predetermined_json_pointer(
            selectors,
            format!("{id_prefix}:blocker:{index}"),
            format!("{pointer_prefix}/blockers/{index}"),
        );
    }
    for (index, _) in receipt.risks.iter().enumerate() {
        push_predetermined_json_pointer(
            selectors,
            format!("{id_prefix}:risk:{index}"),
            format!("{pointer_prefix}/risks/{index}"),
        );
    }
    for (index, result) in receipt.criterion_results.iter().enumerate() {
        if result.status == CriterionStatus::Passed {
            continue;
        }
        push_predetermined_json_pointer(
            selectors,
            format!("{id_prefix}:criterion:{}", result.criterion_id),
            format!("{pointer_prefix}/criterion_results/{index}"),
        );
    }
    if receipt.next_action.is_some() {
        push_predetermined_json_pointer(
            selectors,
            format!("{id_prefix}:next-action"),
            format!("{pointer_prefix}/next_action"),
        );
    }
    push_predetermined_json_pointer(
        selectors,
        format!("{id_prefix}:summary"),
        format!("{pointer_prefix}/summary"),
    );
    for (index, _) in receipt.declared_changes.iter().enumerate() {
        push_predetermined_json_pointer(
            selectors,
            format!("{id_prefix}:declared-change:{index}"),
            format!("{pointer_prefix}/declared_changes/{index}"),
        );
    }
    let mut retained_proofs = 0_usize;
    for (index, call_id) in receipt.validation_call_ids.iter().enumerate() {
        let inline_eligible = call_id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES;
        let retained_inline = inline_eligible && retained_proofs < proof_limit;
        if retained_inline {
            retained_proofs = retained_proofs.saturating_add(1);
        } else {
            push_predetermined_json_pointer(
                selectors,
                format!("{id_prefix}:proof:{call_id}"),
                format!("{pointer_prefix}/validation_call_ids/{index}"),
            );
        }
    }
    if receipt.architecture_contract.is_some() {
        push_predetermined_json_pointer(
            selectors,
            format!("{id_prefix}:architecture-contract"),
            format!("{pointer_prefix}/architecture_contract"),
        );
    }
}

fn receipt_projection_json_pointers(
    receipt: &AgentReceipt,
) -> Vec<codex_tools::ToolOutputProjectionJsonPointer> {
    let mut selectors = Vec::new();
    append_receipt_projection_json_pointers(
        &mut selectors,
        receipt,
        "/receipt",
        "receipt",
        MAX_PROJECTED_PROOF_REFERENCES,
    );
    selectors
}

fn task_projection_json_pointers(
    result: &GetAgentTaskResult,
) -> Vec<codex_tools::ToolOutputProjectionJsonPointer> {
    let task = &result.task;
    let mut selectors = Vec::new();
    if !task.dependency_blockers.is_empty() {
        push_predetermined_json_pointer(&mut selectors, "task:dependency-blockers", "/task/dependency_blockers");
    }
    if !task.criterion_assessments.is_empty() {
        push_predetermined_json_pointer(&mut selectors, "task:criterion-assessments", "/task/criterion_assessments");
    }
    if result.cold_review_context.is_some() {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:target-receipt-handoff",
            "/cold_review_context/target_receipt_handoff",
        );
    }
    if task.assignment.role == AgentRole::Integrator && !result.receipt_handoffs.is_empty() {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:dependency-receipt-handoffs",
            "/dependency_receipt_handoffs",
        );
    }
    for index in 0..task.dependency_admissions.len() {
        push_predetermined_json_pointer(
            &mut selectors,
            format!("task:dependency-admission:{index}"),
            format!("/task/dependency_admissions/{index}"),
        );
    }
    if let Some(receipt) = task.receipt.as_ref() {
        for (index, _) in receipt.blockers.iter().enumerate() {
            push_predetermined_json_pointer(
                &mut selectors,
                format!("task-receipt:blocker:{index}"),
                format!("/task/receipt/blockers/{index}"),
            );
        }
    }
    for (index, gate) in task.gates.iter().enumerate() {
        if matches!(gate.status, GateStatus::Passed | GateStatus::Waived) {
            continue;
        }
        push_predetermined_json_pointer(
            &mut selectors,
            format!("task:unresolved-gate:{index}"),
            format!("/task/gates/{index}"),
        );
    }
    let (criteria, criteria_pointer) = task
        .current_attempt
        .amendment
        .as_ref()
        .and_then(|amendment| amendment.acceptance_criteria.as_deref())
        .map_or(
            (
                task.assignment.acceptance_criteria.as_slice(),
                "/task/assignment/acceptance_criteria",
            ),
            |criteria| {
                (
                    criteria,
                    "/task/current_attempt/amendment/acceptance_criteria",
                )
            },
        );
    let mut retained_unresolved_criteria = 0_usize;
    for (index, criterion) in criteria.iter().enumerate() {
        let passed = task.criterion_is_satisfied(&criterion.id);
        if !passed {
            let inline_eligible = criterion.id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES;
            let retained_inline = inline_eligible
                && retained_unresolved_criteria < MAX_PROJECTED_OBLIGATIONS_PER_KIND;
            if retained_inline {
                retained_unresolved_criteria = retained_unresolved_criteria.saturating_add(1);
            }
            if !retained_inline
                || criterion.text.len() > MAX_PROJECTED_DETAIL_BYTES
                || criterion.evidence_policy.is_some()
            {
                push_predetermined_json_pointer(
                    &mut selectors,
                    format!("task:unresolved-criterion:{}", criterion.id),
                    format!("{criteria_pointer}/{index}"),
                );
            }
        }
    }
    // Recovery selectors expose the requested contract without grading its proof.
    if !task.assignment.required_evidence.is_empty() {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:required-evidence",
            "/task/assignment/required_evidence",
        );
    }
    for (id, pointer) in [
        ("task:objective", "/task/assignment/objective"),
        ("task:stop-condition", "/task/assignment/stop_condition"),
    ] {
        push_predetermined_json_pointer(&mut selectors, id, pointer);
    }
    if task.current_attempt.amendment.is_some() {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:active-amendment",
            "/task/current_attempt/amendment",
        );
    }
    if task
        .workspace_status
        .next_required_action
        .as_ref()
        .is_some_and(|action| action.len() > MAX_PROJECTED_PROSE_BYTES)
    {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:next-required-action",
            "/task/workspace_status/next_required_action",
        );
    }
    for (id_field, json_field, values) in [
        ("read-scope", "read_scope", task.assignment.read_scope.len()),
        (
            "write-scope",
            "write_scope",
            task.assignment.write_scope.len(),
        ),
        (
            "dependency",
            "dependencies",
            task.assignment.dependencies.len(),
        ),
        ("risk-hint", "risk_hints", task.assignment.risk_hints.len()),
        (
            "prohibited-change",
            "prohibited_changes",
            task.assignment.prohibited_changes.len(),
        ),
        (
            "contract-claim",
            "contract_claims",
            task.assignment.contract_claims.len(),
        ),
    ] {
        for index in 0..values {
            push_predetermined_json_pointer(
                &mut selectors,
                format!("task:{id_field}:{index}"),
                format!("/task/assignment/{json_field}/{index}"),
            );
        }
    }
    if task.assignment.relation.is_some() {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:relationship",
            "/task/assignment/relation",
        );
    }
    if task.assignment.architecture_contract_ref.is_some() {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:architecture-contract-reference",
            "/task/assignment/architecture_contract_ref",
        );
    }
    if result.cold_review_context.is_some() {
        push_predetermined_json_pointer(
            &mut selectors,
            "task:cold-review-context",
            "/cold_review_context",
        );
    }
    let observation_start = task
        .observations
        .len()
        .saturating_sub(MAX_PROJECTED_RECENT_OBSERVATIONS);
    for index in observation_start..task.observations.len() {
        push_predetermined_json_pointer(
            &mut selectors,
            format!("task:recent-observation:{index}"),
            format!("/task/observations/{index}"),
        );
    }
    if let Some(receipt) = task.receipt.as_ref() {
        for (index, call) in task.validation_calls.iter().enumerate() {
            if receipt.validation_call_ids.contains(&call.call_id) {
                push_predetermined_json_pointer(
                    &mut selectors,
                    format!("task:validation-call:{}", call.call_id),
                    format!("/task/validation_calls/{index}"),
                );
            }
        }
        append_receipt_projection_json_pointers(
            &mut selectors,
            receipt,
            "/task/receipt",
            "task-receipt",
            0,
        );
    }
    selectors
}

fn bounded_task_projection_metadata<T: Serialize>(
    full: &T,
    mut essential_inline: JsonValue,
    predetermined_json_pointers: Vec<codex_tools::ToolOutputProjectionJsonPointer>,
) -> Option<codex_tools::ToolOutputProjectionMetadata> {
    let spillable = serde_json::to_string(full).ok()?;
    let inline_bytes = serde_json::to_vec(&essential_inline).ok()?.len();
    if inline_bytes > TASK_OUTPUT_INLINE_LIMIT_BYTES {
        tracing::error!(
            inline_bytes,
            inline_limit = TASK_OUTPUT_INLINE_LIMIT_BYTES,
            "bounded task output projection exceeded its hard inline limit"
        );
        essential_inline = json!({
            "assignment_id": essential_inline.get("assignment_id"),
            "attempt_id": essential_inline.get("attempt_id"),
            "status": essential_inline.get("status").or_else(|| essential_inline.get("attempt_state")),
            "projection_truncated": true,
            "omitted_item_counts": essential_inline.get("omitted_item_counts"),
        });
    }
    Some(codex_tools::ToolOutputProjectionMetadata {
        outcome: codex_tools::ToolOutputOutcome::Success,
        diagnostic_class: codex_tools::ToolOutputDiagnosticClass::Normal,
        fragments: Vec::new(),
        spillable_text: vec![spillable],
        essential_inline,
        requested_limit: Some(TASK_OUTPUT_INLINE_LIMIT_BYTES),
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers,
    })
}

#[derive(Debug, Serialize)]
struct SetAgentGateResult {
    gate: AgentGate,
}

#[derive(Debug, Serialize)]
struct AmendAgentTaskResult {
    attempt: Attempt,
}

#[derive(Debug, Serialize)]
struct WaiveAgentGateResult {
    gate: AgentGate,
}

#[derive(Debug, Serialize)]
struct AbandonAgentTaskResult {
    receipt: AgentReceipt,
}

macro_rules! impl_json_output {
    ($output:ty, $tool_name:expr) => {
        impl ToolOutput for $output {
            fn log_preview(&self) -> String {
                tool_output_json_text(self, $tool_name)
            }

            fn success_for_logging(&self) -> bool {
                true
            }

            fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
                crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(
                    self, true,
                )
            }

            fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
                tool_output_response_item(call_id, payload, self, Some(true), $tool_name)
            }

            fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
                tool_output_code_mode_result(self, $tool_name)
            }
        }
    };
}

impl ToolOutput for GetAgentTaskResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, GET_AGENT_TASK_TOOL)
    }

    fn log_output_stats(&self) -> Option<(usize, usize)> {
        let bytes = self.handoff_recovery.as_ref()?.get("canonical_bytes")?.as_u64()?;
        // The retained handoff and log preview serialize the same fields as
        // compact, single-line JSON. Recovery metadata is not serialized there.
        Some((usize::try_from(bytes).ok()?, 1))
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        let projection = task_projection(self);
        if !self.receipt_handoffs.is_empty() {
            // Full evaluation evidence lives in handoff_recovery's artifact;
            // do not automatically drain it back into the summary response.
            return bounded_task_projection_metadata(
                &projection,
                projection.clone(),
                Vec::new(),
            );
        }
        bounded_task_projection_metadata(
            self,
            projection,
            task_projection_json_pointers(self),
        )
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<codex_tools::CanonicalToolResult> {
        if !self.receipt_handoffs.is_empty() {
            return Some(codex_tools::CanonicalToolResult::json(task_projection(self)));
        }
        serde_json::to_value(self)
            .ok()
            .map(codex_tools::CanonicalToolResult::json)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        if !self.receipt_handoffs.is_empty() {
            return tool_output_response_item(
                call_id, payload, &task_projection(self), Some(true), GET_AGENT_TASK_TOOL,
            );
        }
        tool_output_response_item(call_id, payload, self, Some(true), GET_AGENT_TASK_TOOL)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        if !self.receipt_handoffs.is_empty() {
            return task_projection(self);
        }
        tool_output_code_mode_result(self, GET_AGENT_TASK_TOOL)
    }
}

impl ToolOutput for SubmitAgentReceiptResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, SUBMIT_AGENT_RECEIPT_TOOL)
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        bounded_task_projection_metadata(
            self,
            bounded_receipt_projection(&self.receipt),
            receipt_projection_json_pointers(&self.receipt),
        )
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<codex_tools::CanonicalToolResult> {
        serde_json::to_value(self)
            .ok()
            .map(codex_tools::CanonicalToolResult::json)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            self,
            Some(true),
            SUBMIT_AGENT_RECEIPT_TOOL,
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, SUBMIT_AGENT_RECEIPT_TOOL)
    }
}
impl_json_output!(SetAgentGateResult, SET_AGENT_GATE_TOOL);
impl_json_output!(AmendAgentTaskResult, AMEND_AGENT_TASK_TOOL);
impl_json_output!(WaiveAgentGateResult, WAIVE_AGENT_GATE_TOOL);
impl_json_output!(AbandonAgentTaskResult, ABANDON_AGENT_TASK_TOOL);

fn get_agent_task_spec() -> ToolSpec {
    function_spec(
        GET_AGENT_TASK_TOOL,
        "Read a durable task's current attempt, gates, receipt, evidence and recent observations. \
         Bound Reviewers, Verifiers and Integrators use their OWN assignment_id for compact \
         target/dependency handoffs. Recover only needed contract, receipt or evidence fields \
         with read_tool_output using canonical_recovery.artifact_id and the supplied JSON pointers. \
         Summaries and worker declarations are not proof or workspace-freshness claims; null \
         receipts mean unavailable. Current attempts are separate from pinned admission provenance. \
         Handoffs include no transcripts and grant no direct target/dependency/sibling access. \
         Non-root callers may also read their immediate compatibility children's tasks.",
        object_schema(
            [
                (
                    "assignment_id",
                    JsonSchema::string(Some(
                        "UUIDv7 assignment identifier from spawn_agent; use your own ID for evaluation handoffs.".to_string(),
                    )),
                ),
                (
                    "observation_limit",
                    JsonSchema::integer(Some(
                        "Number of newest observations to return. Defaults to 20; capped at 100."
                            .to_string(),
                    )),
                ),
            ],
            &["assignment_id"],
        ),
    )
}

fn submit_agent_receipt_spec() -> ToolSpec {
    let architecture_criterion = object_schema(
        [
            (
                "evidence_policy",
                crate::tools::handlers::multi_agents_spec::criterion_evidence_policy_schema(),
            ),
            (
                "id",
                JsonSchema::string(Some("Stable acceptance-criterion id.".to_string())),
            ),
            (
                "text",
                JsonSchema::string(Some("Required observable outcome.".to_string())),
            ),
        ],
        &["id", "text"],
    );
    let architecture_scope = object_schema(
        [
            (
                "path",
                JsonSchema::string(Some("Repository-relative path.".to_string())),
            ),
            (
                "recursive",
                JsonSchema::boolean(Some(
                    "Whether this scope includes descendants of path.".to_string(),
                )),
            ),
        ],
        &["path", "recursive"],
    );
    let architecture_contract = object_schema(
        [
            (
                "schema_version",
                JsonSchema::integer(Some(
                    "ArchitectureContractV1 version; currently 1.".to_string(),
                )),
            ),
            (
                "objective",
                JsonSchema::string(Some("Authoritative worker objective.".to_string())),
            ),
            (
                "acceptance_criteria",
                JsonSchema::array(
                    architecture_criterion,
                    Some("Authoritative worker acceptance criteria.".to_string()),
                ),
            ),
            (
                "read_scope",
                JsonSchema::array(
                    architecture_scope.clone(),
                    Some("Canonical worker read scope.".to_string()),
                ),
            ),
            (
                "write_scope",
                JsonSchema::array(
                    architecture_scope,
                    Some("Canonical worker write scope.".to_string()),
                ),
            ),
            (
                "stop_condition",
                JsonSchema::string(Some("Authoritative worker stop condition.".to_string())),
            ),
            (
                "risk_hints",
                string_array_schema("Canonical worker risk hints."),
            ),
            (
                "required_evidence",
                string_array_schema(
                    "Canonical worker evidence requirements. inspect:<normalized repository-relative file> requests a complete native read_file source_inspection receipt, not a test or semantic correctness claim.",
                ),
            ),
            (
                "prohibited_changes",
                string_array_schema("Canonical worker prohibitions."),
            ),
            (
                "contract_claims",
                string_array_schema("Canonical worker contract claims."),
            ),
        ],
        &[
            "schema_version",
            "objective",
            "acceptance_criteria",
            "read_scope",
            "write_scope",
            "stop_condition",
            "risk_hints",
            "required_evidence",
            "prohibited_changes",
            "contract_claims",
        ],
    );
    let criterion_result = object_schema(
        [
            (
                "criterion_id",
                JsonSchema::string(Some(
                    "Acceptance-criterion id from the current assignment or amendment.".to_string(),
                )),
            ),
            (
                "status",
                enum_schema(
                    ["passed", "failed", "not_run"],
                    "Result for this acceptance criterion.",
                ),
            ),
            (
                "evidence",
                JsonSchema::string(Some(
                    "Optional short explanation when the evidence needs interpretation. Reuse evidence_ref across criteria instead of repeating command results; without a reference the reported outcome remains unverified.".to_string(),
                )),
            ),
            (
                "evidence_ref",
                object_schema([
                    ("call_id", JsonSchema::string(Some("Successful owned validation ID; included automatically when validation_call_ids is omitted.".to_string()))),
                    ("workspace_id", JsonSchema::string(Some("Omit to use the bound assignment's workspace.".to_string()))),
                    ("evidence_epoch", JsonSchema::integer(Some("Omit to use the validation call's recorded end_epoch.".to_string()))),
                    ("kind", enum_schema(["validation_execution", "source_inspection"], "Omit to use the recorded producer kind. Source inspection proves a complete hash-bound read, not semantic correctness; execution alone does not establish test coverage or deployment.")),
                ], &["call_id"]),
            ),
        ],
        &["criterion_id", "status"],
    );
    let declared_change = object_schema(
        [
            (
                "path",
                JsonSchema::string(Some("Repository-relative changed path.".to_string())),
            ),
            (
                "summary",
                JsonSchema::string(Some("Concise description of the change.".to_string())),
            ),
        ],
        &["path", "summary"],
    );

    function_spec(
        SUBMIT_AGENT_RECEIPT_TOOL,
        "Seal the calling typed agent's receipt against its bound assignment and current attempt. \
         The caller cannot select an assignment or attempt, and a sealed receipt is immutable.",
        object_schema(
            [
                (
                    "status",
                    enum_schema(
                        [
                            "completed",
                            "needs_main",
                            "blocked",
                            "failed",
                            "violated",
                            "abandoned",
                        ],
                        "Agent's terminal status claim.",
                    ),
                ),
                (
                    "summary",
                    JsonSchema::string(Some("Concise final task summary.".to_string())),
                ),
                (
                    "criterion_results",
                    JsonSchema::array(
                        criterion_result,
                        Some("One result for every effective acceptance criterion.".to_string()),
                    ),
                ),
                (
                    "declared_changes",
                    JsonSchema::array(
                        declared_change,
                        Some("Repository changes attributed to this attempt.".to_string()),
                    ),
                ),
                (
                    "validation_call_ids",
                    string_array_schema(
                        "Completed validation tool-call ids owned by this attempt. Omit to use the distinct calls cited by criterion evidence_ref fields; include a list only to select additional or different calls.",
                    ),
                ),
                (
                    "blockers",
                    string_array_schema("Blockers that prevented completion. Omit when empty."),
                ),
                (
                    "risks",
                    string_array_schema("Known remaining risks or uncertainties. Omit when empty."),
                ),
                (
                    "next_action",
                    JsonSchema::string(Some(
                        "Include only when the root agent needs a concrete follow-up action."
                            .to_string(),
                    )),
                ),
                ("architecture_contract", architecture_contract),
            ],
            &["status", "summary", "criterion_results", "declared_changes"],
        ),
    )
}

fn set_agent_gate_spec() -> ToolSpec {
    function_spec(
        SET_AGENT_GATE_TOOL,
        "Submit an evidence-backed gate verdict. Reviewers may set only review gates for their declared targets; verifiers may set only verification gates for their declared targets. Root may set any non-waiver gate. Waivers remain root-only through waive_agent_gate.",
        object_schema(
            [
                ("assignment_id", assignment_id_schema()),
                ("expected_attempt_id", JsonSchema::string(Some("Exact target attempt_id from the task or cold review context you evaluated. Stale verdicts are rejected; recover and evaluate the current attempt before retrying.".to_string()))),
                (
                    "gate",
                    enum_schema(
                        ["risk", "review", "verification", "mutation", "ownership"],
                        "Gate kind to evaluate.",
                    ),
                ),
                (
                    "status",
                    enum_schema(
                        [
                            "pending",
                            "passed",
                            "changes_requested",
                            "failed",
                            "violated",
                        ],
                        "Gate verdict. Waived is intentionally unavailable here.",
                    ),
                ),
                (
                    "reason",
                    JsonSchema::string(Some(
                        "Concise evidence-backed reason for the verdict.".to_string(),
                    )),
                ),
            ],
            &["assignment_id", "expected_attempt_id", "gate", "status", "reason"],
        ),
    )
}

fn amend_agent_task_spec() -> ToolSpec {
    let criterion = object_schema(
        [
            (
                "evidence_policy",
                crate::tools::handlers::multi_agents_spec::criterion_evidence_policy_schema(),
            ),
            (
                "id",
                JsonSchema::string(Some("Stable criterion id.".to_string())),
            ),
            (
                "text",
                JsonSchema::string(Some("Required outcome for this criterion.".to_string())),
            ),
        ],
        &["id", "text"],
    );
    function_spec(
        AMEND_AGENT_TASK_TOOL,
        "Root-only. Create the one allowed correction attempt for a sealed worker assignment \
         after its review gate requests changes. This updates the durable contract and binding, \
         but does not notify or start the worker. Use followup_task to request the correction; \
         the worker must recover its current attempt and effective contract before acting.",
        object_schema(
            [
                ("assignment_id", assignment_id_schema()),
                (
                    "reason",
                    JsonSchema::string(Some("Why a correction attempt is required.".to_string())),
                ),
                (
                    "objective",
                    JsonSchema::string(Some(
                        "Replacement objective for the correction attempt.".to_string(),
                    )),
                ),
                (
                    "acceptance_criteria",
                    JsonSchema::array(
                        criterion,
                        Some(
                            "Replacement acceptance criteria for the correction attempt."
                                .to_string(),
                        ),
                    ),
                ),
                (
                    "stop_condition",
                    JsonSchema::string(Some(
                        "Replacement stop condition for the correction attempt.".to_string(),
                    )),
                ),
            ],
            &["assignment_id", "reason"],
        ),
    )
}

fn waive_agent_gate_spec() -> ToolSpec {
    function_spec(
        WAIVE_AGENT_GATE_TOOL,
        "Root-only. Waive a pending soft review or verification gate with an explicit reason. \
         Other gate kinds cannot be waived.",
        object_schema(
            [
                ("assignment_id", assignment_id_schema()),
                (
                    "gate",
                    enum_schema(["review", "verification"], "Pending soft gate to waive."),
                ),
                (
                    "reason",
                    JsonSchema::string(Some("Why the gate is safe to waive.".to_string())),
                ),
            ],
            &["assignment_id", "gate", "reason"],
        ),
    )
}

fn abandon_agent_task_spec() -> ToolSpec {
    function_spec(
        ABANDON_AGENT_TASK_TOOL,
        "Root-only. Seal the assignment's current active attempt as abandoned and release its \
         task lease.",
        object_schema(
            [
                ("assignment_id", assignment_id_schema()),
                (
                    "reason",
                    JsonSchema::string(Some("Why the active task is being abandoned.".to_string())),
                ),
            ],
            &["assignment_id", "reason"],
        ),
    )
}

fn function_spec(name: &str, description: &str, parameters: JsonSchema) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters,
        output_schema: None,
    })
}

fn assignment_id_schema() -> JsonSchema {
    JsonSchema::string(Some(
        "UUIDv7 assignment identifier returned by spawn_agent.".to_string(),
    ))
}

fn string_array_schema(description: &str) -> JsonSchema {
    JsonSchema::array(JsonSchema::string(None), Some(description.to_string()))
}

fn enum_schema<const N: usize>(values: [&str; N], description: &str) -> JsonSchema {
    JsonSchema::string_enum(
        values
            .into_iter()
            .map(|value| JsonValue::String(value.to_string()))
            .collect(),
        Some(description.to_string()),
    )
}

fn object_schema<const N: usize>(
    properties: [(&str, JsonSchema); N],
    required: &[&str],
) -> JsonSchema {
    JsonSchema::object(
        properties
            .into_iter()
            .map(|(name, schema)| (name.to_string(), schema))
            .collect(),
        Some(required.iter().map(|name| (*name).to_string()).collect()),
        Some(false.into()),
    )
}


