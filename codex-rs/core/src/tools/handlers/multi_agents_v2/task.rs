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
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::ArchitectureContractV1;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptAmendment;
use codex_agent_task_store::AttributionConfidence;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::DEFAULT_OBSERVATION_LIMIT;
use codex_agent_task_store::DeclaredChange;
use codex_agent_task_store::GateKind;
use codex_agent_task_store::GateStatus;
use codex_agent_task_store::MAX_MUTATION_EVIDENCE_LIMIT;
use codex_agent_task_store::MAX_OBSERVATION_LIMIT;
use codex_agent_task_store::MAX_SNAPSHOT_CHUNK_BYTES;
use codex_agent_task_store::MutationEvidence;
use codex_agent_task_store::MutationSnapshotVersion;
use codex_agent_task_store::ReceiptDraft;
use codex_agent_task_store::RelationKind;
use codex_agent_task_store::RepoScope;
use codex_agent_task_store::RiskDomain;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::TaskActor;
use codex_agent_task_store::TaskCapsuleV1;
use codex_agent_task_store::ValidationCall;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationProofKind;
use codex_git_utils::get_git_repo_root;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use similar::ChangeTag;
use similar::TextDiff;
use std::collections::BTreeSet;
use std::path::Path;

const GET_AGENT_TASK_TOOL: &str = "get_agent_task";
const SUBMIT_AGENT_RECEIPT_TOOL: &str = "submit_agent_receipt";
const SET_AGENT_GATE_TOOL: &str = "set_agent_gate";
const AMEND_AGENT_TASK_TOOL: &str = "amend_agent_task";
const WAIVE_AGENT_GATE_TOOL: &str = "waive_agent_gate";
const ABANDON_AGENT_TASK_TOOL: &str = "abandon_agent_task";
const MAX_COLD_REVIEW_DIFF_BYTES: usize = 256 * 1024;
const MAX_COLD_REVIEW_FILE_BYTES: usize = 1024 * 1024;

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
        turn,
        payload,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: GetAgentTaskArgs = parse_arguments(&arguments)?;
    let assignment_id = parse_assignment_id(GET_AGENT_TASK_TOOL, &args.assignment_id)?;
    let observation_limit = args.observation_limit.unwrap_or(DEFAULT_OBSERVATION_LIMIT);
    if observation_limit > MAX_OBSERVATION_LIMIT {
        return Err(FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: observation_limit must be between 0 and \
             {MAX_OBSERVATION_LIMIT}, got {observation_limit}"
        )));
    }

    let coordinator = session.services.agent_control.task_coordinator();
    let caller_binding = if turn.session_source.is_non_root_agent() {
        let binding = coordinator
            .binding_for_source(&turn.session_source)
            .filter(|binding| binding.assignment_id == assignment_id)
            .ok_or_else(|| {
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
    if caller_binding
        .as_ref()
        .is_some_and(|binding| binding.attempt_id != task.current_attempt.attempt_id)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: non-root callers may only read their own current bound task"
        )));
    }
    let cold_review_context = if caller_binding.is_some() {
        build_evaluation_context(session.as_ref(), turn.config.cwd.as_path(), &task).await?
    } else {
        None
    };
    coordinator
        .maybe_emit_terminal_metrics(assignment_id, &turn.session_telemetry)
        .await;
    Ok(boxed_tool_output(GetAgentTaskResult {
        task,
        cold_review_context,
    }))
}

async fn handle_submit_agent_receipt(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: SubmitAgentReceiptArgs = parse_arguments(&arguments)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let binding = coordinator
        .binding_for_source(&turn.session_source)
        .ok_or_else(|| {
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
    let workspace_root = get_git_repo_root(turn.config.cwd.as_path())
        .unwrap_or_else(|| turn.config.cwd.to_path_buf());
    let _workspace_operation_permit =
        crate::workspace_operation_gate::acquire_workspace_operation(&workspace_root).await;
    let draft = args.into_receipt_draft();
    if let Some(obligations) = store
        .replay_required_evidence_missing(binding.attempt_id, &draft)
        .await
        .map_err(|error| task_store_error(SUBMIT_AGENT_RECEIPT_TOOL, error))?
    {
        return Err(task_store_error(
            SUBMIT_AGENT_RECEIPT_TOOL,
            StoreError::RequiredEvidenceMissing { obligations },
        ));
    }
    if let Err(error) = store.finalize_pending_mutations(binding.attempt_id).await {
        tracing::warn!(
            %error,
            attempt_id = %binding.attempt_id,
            "typed mutation evidence finalization was unavailable; continuing receipt submission"
        );
    }
    // Risk derivation and cold-review evidence must cover the complete attempt, including writes
    // that another runtime path finalized before receipt submission.
    let observed_writes = match store
        .list_mutation_evidence(binding.attempt_id, Some(MAX_MUTATION_EVIDENCE_LIMIT))
        .await
    {
        Ok(observed_writes) => observed_writes,
        Err(error) => {
            tracing::warn!(
                %error,
                attempt_id = %binding.attempt_id,
                "typed mutation evidence could not be read; continuing receipt submission"
            );
            Vec::new()
        }
    };
    let review_reason = derive_review_reason(
        store.as_ref(),
        turn.config.cwd.as_path(),
        &task,
        &draft,
        &observed_writes,
    )
    .await?;
    let receipt = match review_reason {
        Some(review_reason) => {
            store
                .submit_agent_receipt_with_review(binding.attempt_id, draft, review_reason)
                .await
        }
        None => store.submit_agent_receipt(binding.attempt_id, draft).await,
    }
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

struct SnapshotContent {
    existed: bool,
    bytes: Option<Vec<u8>>,
    total_bytes: u64,
}

#[derive(Default)]
struct AttemptDiffSummary {
    text: String,
    changed_paths: Vec<String>,
    non_generated_changed_files: u32,
    non_generated_changed_lines: u32,
}

async fn derive_review_reason(
    store: &dyn AgentTaskStore,
    cwd: &Path,
    task: &AgentTask,
    draft: &ReceiptDraft,
    observed_writes: &[MutationEvidence],
) -> Result<Option<String>, FunctionCallError> {
    if draft.status != AgentStatusClaim::Completed {
        return Ok(None);
    }

    let repo_root = get_git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let diff = build_attempt_diff(store, task.current_attempt.attempt_id, observed_writes)
        .await
        .map_err(|error| task_store_error(SUBMIT_AGENT_RECEIPT_TOOL, error))?;
    let risk_hints = parse_risk_hints(&task.assignment.risk_hints);
    let successful_validation_ids = task
        .validation_calls
        .iter()
        .filter(|call| is_successful_focused_validation(call))
        .map(|call| call.call_id.as_str())
        .collect::<BTreeSet<_>>();
    let focused_validation_succeeded = !draft.validation_call_ids.is_empty()
        && draft
            .validation_call_ids
            .iter()
            .all(|call_id| successful_validation_ids.contains(call_id.as_str()));
    let touched_contracts = if diff.changed_paths.is_empty() {
        Vec::new()
    } else {
        risk_hints.contracts.clone()
    };
    let drift = observed_writes
        .iter()
        .any(|evidence| evidence.attribution_confidence == AttributionConfidence::DetectionOnly);
    let derived = derive_risk_policy(
        &task.assignment,
        &repo_root,
        RiskPolicyInput {
            changed_paths: &diff.changed_paths,
            configured_high_risk_paths: &risk_hints.high_risk_paths,
            touched_contracts: &touched_contracts,
            configured_high_risk_contracts: &risk_hints.contracts,
            cross_owner_scope: false,
            named_domains: &risk_hints.domains,
            non_generated_changed_files: diff.non_generated_changed_files,
            non_generated_changed_lines: diff.non_generated_changed_lines,
            focused_validation_succeeded,
            // Claim overlap is advisory metadata, not an ownership conflict. Detection-only
            // attribution remains the persisted signal that concurrent drift may exist.
            ownership_conflict: false,
            drift,
        },
    )
    .map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "{SUBMIT_AGENT_RECEIPT_TOOL}: risk evidence is invalid: {error}"
        ))
    })?;
    Ok(derived.decision.review_required.then(|| {
        format!(
            "cold review required: {}",
            derived.decision.reasons.join("; ")
        )
    }))
}

fn is_successful_focused_validation(call: &ValidationCall) -> bool {
    call.proof_kind == ValidationProofKind::Focused
        && call
            .resolved_executable
            .as_deref()
            .is_some_and(|path| Path::new(path).is_absolute())
        && call.status == ValidationCallStatus::Succeeded
}

async fn build_evaluation_context(
    session: &crate::session::session::Session,
    cwd: &Path,
    task: &AgentTask,
) -> Result<Option<ColdReviewContext>, FunctionCallError> {
    let (role_name, relation_kind) = match task.assignment.role {
        AgentRole::Reviewer => ("reviewer", RelationKind::Review),
        AgentRole::Verifier => ("verifier", RelationKind::Verification),
        _ => return Ok(None),
    };
    let relation = task.assignment.relation.as_ref().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: {role_name} assignment is missing its evaluation target"
        ))
    })?;
    if relation.kind != relation_kind || relation.target_assignment_ids.len() != 1 {
        return Err(FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: {role_name} assignment has an invalid evaluation relation"
        )));
    }
    let target_assignment_id = relation.target_assignment_ids[0];
    let coordinator = session.services.agent_control.task_coordinator();
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: the typed task store is unavailable"
        ))
    })?;
    let target = store
        .get_agent_task(target_assignment_id, Some(0))
        .await
        .map_err(|error| task_store_error(GET_AGENT_TASK_TOOL, error))?;
    let observed_writes = store
        .list_mutation_evidence(
            target.current_attempt.attempt_id,
            Some(MAX_MUTATION_EVIDENCE_LIMIT),
        )
        .await
        .map_err(|error| task_store_error(GET_AGENT_TASK_TOOL, error))?;
    let diff = build_attempt_diff(
        store.as_ref(),
        target.current_attempt.attempt_id,
        &observed_writes,
    )
    .await
    .map_err(|error| task_store_error(GET_AGENT_TASK_TOOL, error))?;
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
    let relevant_contracts = parse_risk_hints(&target.assignment.risk_hints).contracts;
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
    let repo_root = get_git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    build_cold_review_context(
        &repo_root,
        ColdReviewContextInput {
            assignment: target.assignment,
            attempt_id: target.current_attempt.attempt_id,
            applicable_instructions,
            attempt_specific_diff: diff.text,
            observed_writes,
            relevant_contracts,
            nearest_tests,
        },
    )
    .map(Some)
    .map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "{GET_AGENT_TASK_TOOL}: cold-review evidence is invalid: {error}"
        ))
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

async fn build_attempt_diff(
    store: &dyn AgentTaskStore,
    attempt_id: codex_agent_task_store::AttemptId,
    observed_writes: &[MutationEvidence],
) -> Result<AttemptDiffSummary, StoreError> {
    let mut summary = AttemptDiffSummary::default();
    let mut truncated = false;
    for evidence in observed_writes {
        let final_existed = evidence.final_write_existed.unwrap_or(false);
        if evidence.pre_write_hash == evidence.final_hash
            && evidence.pre_write_existed == final_existed
        {
            continue;
        }
        summary.changed_paths.push(evidence.path.clone());
        let (section, changed_lines, generated) = if evidence.snapshot_retained {
            let before = read_snapshot(
                store,
                attempt_id,
                &evidence.path,
                MutationSnapshotVersion::PreWrite,
            )
            .await?;
            let after = read_snapshot(
                store,
                attempt_id,
                &evidence.path,
                MutationSnapshotVersion::Final,
            )
            .await?;
            render_snapshot_diff(&evidence.path, &before, &after)
        } else {
            (
                format!(
                    "diff --git a/{0} b/{0}\n[private mutation snapshot unavailable]\n",
                    evidence.path
                ),
                401,
                false,
            )
        };
        if !generated {
            summary.non_generated_changed_files =
                summary.non_generated_changed_files.saturating_add(1);
            summary.non_generated_changed_lines = summary
                .non_generated_changed_lines
                .saturating_add(changed_lines);
        }
        push_bounded_diff(&mut summary.text, &section, &mut truncated);
    }
    if truncated {
        const NOTICE: &str = "\n[attempt-specific diff truncated; write hashes remain available]\n";
        let keep = MAX_COLD_REVIEW_DIFF_BYTES.saturating_sub(NOTICE.len());
        summary
            .text
            .truncate(floor_char_boundary(&summary.text, keep));
        summary.text.push_str(NOTICE);
    }
    Ok(summary)
}

async fn read_snapshot(
    store: &dyn AgentTaskStore,
    attempt_id: codex_agent_task_store::AttemptId,
    path: &str,
    version: MutationSnapshotVersion,
) -> Result<SnapshotContent, StoreError> {
    let first = store
        .read_mutation_snapshot(
            attempt_id,
            path.to_string(),
            version,
            0,
            Some(MAX_SNAPSHOT_CHUNK_BYTES),
        )
        .await?;
    if first.total_bytes > MAX_COLD_REVIEW_FILE_BYTES as u64 {
        return Ok(SnapshotContent {
            existed: first.existed,
            bytes: None,
            total_bytes: first.total_bytes,
        });
    }
    let mut bytes = first.bytes;
    let existed = first.existed;
    let total_bytes = first.total_bytes;
    let mut next_offset = first.next_offset;
    while let Some(offset) = next_offset {
        let chunk = store
            .read_mutation_snapshot(
                attempt_id,
                path.to_string(),
                version,
                offset,
                Some(MAX_SNAPSHOT_CHUNK_BYTES),
            )
            .await?;
        if chunk.existed != existed || chunk.total_bytes != total_bytes {
            return Err(StoreError::CorruptData(format!(
                "snapshot metadata changed while reading {path}"
            )));
        }
        bytes.extend_from_slice(&chunk.bytes);
        next_offset = chunk.next_offset;
    }
    Ok(SnapshotContent {
        existed,
        bytes: Some(bytes),
        total_bytes,
    })
}

fn render_snapshot_diff(
    path: &str,
    before: &SnapshotContent,
    after: &SnapshotContent,
) -> (String, u32, bool) {
    let (Some(before_bytes), Some(after_bytes)) = (&before.bytes, &after.bytes) else {
        return (
            format!(
                "diff --git a/{0} b/{0}\n[snapshot exceeds cold-review limit: before={1} bytes, after={2} bytes]\n",
                path, before.total_bytes, after.total_bytes
            ),
            401,
            false,
        );
    };
    let generated = confirmed_generated(before, before_bytes, after, after_bytes);
    let (Ok(before_text), Ok(after_text)) = (
        std::str::from_utf8(before_bytes),
        std::str::from_utf8(after_bytes),
    ) else {
        return (
            format!("diff --git a/{path} b/{path}\nBinary files differ\n"),
            401,
            generated,
        );
    };
    let text_diff = TextDiff::from_lines(before_text, after_text);
    let changed_lines = text_diff
        .iter_all_changes()
        .filter(|change| change.tag() != ChangeTag::Equal)
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    let old_header = if before.existed {
        format!("a/{path}")
    } else {
        "/dev/null".to_string()
    };
    let new_header = if after.existed {
        format!("b/{path}")
    } else {
        "/dev/null".to_string()
    };
    let mut section = format!("diff --git a/{path} b/{path}\n");
    section.push_str(
        &text_diff
            .unified_diff()
            .context_radius(3)
            .header(&old_header, &new_header)
            .to_string(),
    );
    if !section.ends_with('\n') {
        section.push('\n');
    }
    (section, changed_lines, generated)
}

fn confirmed_generated(
    before: &SnapshotContent,
    before_bytes: &[u8],
    after: &SnapshotContent,
    after_bytes: &[u8],
) -> bool {
    (!before.existed || generated_marker(before_bytes))
        && (!after.existed || generated_marker(after_bytes))
}

fn generated_marker(contents: &[u8]) -> bool {
    let prefix = &contents[..contents.len().min(4096)];
    let lowercase = String::from_utf8_lossy(prefix).to_ascii_lowercase();
    lowercase.contains("@generated")
        || lowercase.contains("code generated") && lowercase.contains("do not edit")
        || lowercase.contains("automatically generated") && lowercase.contains("do not edit")
}

fn push_bounded_diff(output: &mut String, section: &str, truncated: &mut bool) {
    if output.len() >= MAX_COLD_REVIEW_DIFF_BYTES {
        *truncated = true;
        return;
    }
    let remaining = MAX_COLD_REVIEW_DIFF_BYTES - output.len();
    if section.len() <= remaining {
        output.push_str(section);
    } else {
        output.push_str(&section[..floor_char_boundary(section, remaining)]);
        *truncated = true;
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

async fn handle_set_agent_gate(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: SetAgentGateArgs = parse_arguments(&arguments)?;
    let assignment_id = parse_assignment_id(SET_AGENT_GATE_TOOL, &args.assignment_id)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let actor = if turn.session_source.is_non_root_agent() {
        let binding = coordinator
            .binding_for_source(&turn.session_source)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "{SET_AGENT_GATE_TOOL}: the caller is not a typed agent with a bound task"
                ))
            })?;
        TaskActor::Attempt(binding.attempt_id)
    } else {
        TaskActor::Root
    };
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{SET_AGENT_GATE_TOOL}: the typed task store is unavailable"
        ))
    })?;
    let gate = store
        .set_agent_gate(actor, assignment_id, args.gate, args.status, args.reason)
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
        turn,
        payload,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: AmendAgentTaskArgs = parse_arguments(&arguments)?;
    require_root(&turn.session_source, AMEND_AGENT_TASK_TOOL)?;
    let assignment_id = parse_assignment_id(AMEND_AGENT_TASK_TOOL, &args.assignment_id)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{AMEND_AGENT_TASK_TOOL}: the typed task store is unavailable"
        ))
    })?;
    let attempt = store
        .amend_agent_task(TaskActor::Root, assignment_id, args.into_amendment())
        .await
        .map_err(|error| task_store_error(AMEND_AGENT_TASK_TOOL, error))?;

    // A correction attempt remains assigned to the same typed agent. Refresh both the durable
    // binding and the coordinator cache so mutation attribution and receipt fallback use it.
    let binding = match coordinator.binding_for_assignment(assignment_id) {
        Some(binding) => Some(binding),
        None => coordinator
            .get_agent_task_binding(assignment_id)
            .await
            .map_err(|error| task_store_error(AMEND_AGENT_TASK_TOOL, error))?,
    };
    if let Some(binding) = binding {
        coordinator
            .bind_agent_task(AgentTaskBindingDraft {
                assignment_id,
                attempt_id: attempt.attempt_id,
                agent_path: binding.agent_path,
                task_name: binding.task_name,
                thread_id: binding.thread_id,
            })
            .await
            .map_err(|error| task_store_error(AMEND_AGENT_TASK_TOOL, error))?;
    }

    Ok(boxed_tool_output(AmendAgentTaskResult { attempt }))
}

async fn handle_waive_agent_gate(
    invocation: ToolInvocation,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
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
            TaskActor::Root,
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
        turn,
        payload,
        ..
    } = invocation;
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
        .abandon_agent_task(TaskActor::Root, assignment_id, args.reason)
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
    let detail = match error {
        StoreError::RequiredEvidenceMissing { obligations } => serde_json::json!({
            "kind": "required_evidence_missing",
            "obligations": obligations,
        })
        .to_string(),
        StoreError::Io(_)
        | StoreError::Sql(_)
        | StoreError::Migration(_)
        | StoreError::Json(_)
        | StoreError::CorruptData(_) => {
            "the typed task store is unavailable or contains invalid persisted state".to_string()
        }
        error => error.to_string(),
    };
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
    validation_call_ids: Vec<String>,
    blockers: Vec<String>,
    risks: Vec<String>,
    next_action: Option<String>,
    #[serde(default)]
    architecture_contract: Option<ArchitectureContractV1>,
}

impl SubmitAgentReceiptArgs {
    fn into_receipt_draft(self) -> ReceiptDraft {
        ReceiptDraft {
            status: self.status,
            summary: self.summary,
            criterion_results: self
                .criterion_results
                .into_iter()
                .map(ReceiptCriterionArgs::into_criterion_result)
                .collect(),
            declared_changes: self
                .declared_changes
                .into_iter()
                .map(DeclaredChangeArgs::into_declared_change)
                .collect(),
            validation_call_ids: self.validation_call_ids,
            blockers: self.blockers,
            risks: self.risks,
            next_action: self.next_action,
            architecture_contract: self.architecture_contract,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptCriterionArgs {
    criterion_id: String,
    status: CriterionStatus,
    evidence: Option<String>,
}

impl ReceiptCriterionArgs {
    fn into_criterion_result(self) -> CriterionResult {
        CriterionResult {
            criterion_id: self.criterion_id,
            status: self.status,
            evidence: self.evidence,
        }
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
}

impl AcceptanceCriterionArgs {
    fn into_acceptance_criterion(self) -> AcceptanceCriterion {
        AcceptanceCriterion {
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

#[derive(Debug, Serialize)]
struct GetAgentTaskResult {
    task: AgentTask,
    #[serde(skip_serializing_if = "Option::is_none")]
    cold_review_context: Option<ColdReviewContext>,
}

#[derive(Debug, Serialize)]
struct SubmitAgentReceiptResult {
    receipt: AgentReceipt,
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
        "evidence_manifest_hash": (receipt.evidence_manifest_hash.len() <= MAX_PROJECTED_EXACT_STRING_BYTES).then_some(receipt.evidence_manifest_hash.as_str()),
        "sealed_at": receipt.sealed_at,
        "proof_references": proof_references,
        "changed_paths": changed_paths,
        "criterion_counts": {
            "total": receipt.criterion_results.len(),
            "passed": passed,
            "failed": failed,
            "not_run": receipt.criterion_results.len().saturating_sub(passed + failed),
        },
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

fn required_evidence_is_satisfied(task: &AgentTask, requirement: &str) -> bool {
    task.validation_calls.iter().any(|call| {
        task.receipt
            .as_ref()
            .is_some_and(|receipt| receipt.validation_call_ids.contains(&call.call_id))
            && call.command_summary == requirement
            && call.status == ValidationCallStatus::Succeeded
            && call.proof_kind == ValidationProofKind::Focused
            && call.evidence.end_epoch.is_some()
            && call.evidence.stale_reason.is_none()
            && call
                .resolved_executable
                .as_deref()
                .is_some_and(|path| Path::new(path).is_absolute())
    })
}

fn task_projection(result: &GetAgentTaskResult) -> JsonValue {
    let task = &result.task;
    let capsule = task
        .assignment
        .task_capsule
        .as_deref()
        .and_then(|payload| serde_json::from_str::<TaskCapsuleV1>(payload).ok());
    let proof_references = task
        .validation_calls
        .iter()
        .filter_map(|call| {
            if call.call_id.len() > MAX_PROJECTED_EXACT_STRING_BYTES {
                return None;
            }
            Some(json!({
                "call_id": call.call_id,
                "status": call.status,
                "retained_output_ref": call.evidence.retained_output_ref.as_deref().filter(|value| value.len() <= MAX_PROJECTED_EXACT_STRING_BYTES),
                "shared_from_call_id": call.evidence.shared_from_call_id.as_deref().filter(|value| value.len() <= MAX_PROJECTED_EXACT_STRING_BYTES),
            }))
        })
        .take(MAX_PROJECTED_VALIDATION_CALLS)
        .collect::<Vec<_>>();
    let omitted_validation_calls = task
        .validation_calls
        .len()
        .saturating_sub(proof_references.len());
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
            if status == Some(CriterionStatus::Passed)
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
        .filter(|criterion| {
            task.receipt.as_ref().and_then(|receipt| {
                receipt
                    .criterion_results
                    .iter()
                    .find(|result| result.criterion_id == criterion.id)
                    .map(|result| result.status)
            }) != Some(CriterionStatus::Passed)
        })
        .count();
    let unresolved_required_evidence = task
        .assignment
        .required_evidence
        .iter()
        .enumerate()
        .filter(|(_, requirement)| !required_evidence_is_satisfied(task, requirement))
        .map(|(ordinal, requirement)| {
            json!({
                "ordinal": ordinal + 1,
                "requirement": bounded_detail(Some(requirement)),
            })
        })
        .take(MAX_PROJECTED_OBLIGATIONS_PER_KIND)
        .collect::<Vec<_>>();
    let unresolved_required_evidence_total = task
        .assignment
        .required_evidence
        .iter()
        .filter(|requirement| !required_evidence_is_satisfied(task, requirement))
        .count();
    let omitted_observations = task
        .observations
        .len()
        .saturating_sub(recent_observations.len());
    let omitted_acceptance_criteria = criteria.len().saturating_sub(unresolved_criteria.len());
    let omitted_unresolved_criteria =
        unresolved_criteria_total.saturating_sub(unresolved_criteria.len());
    let omitted_required_evidence = task
        .assignment
        .required_evidence
        .len()
        .saturating_sub(unresolved_required_evidence.len());
    let omitted_unresolved_required_evidence =
        unresolved_required_evidence_total.saturating_sub(unresolved_required_evidence.len());
    json!({
        "assignment_id": task.assignment.assignment_id,
        "attempt_id": task.current_attempt.attempt_id,
        "role": task.assignment.role,
        "attempt_state": task.current_attempt.state,
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
            "required_evidence": unresolved_required_evidence,
        },
        "recent_observations": recent_observations,
        "receipt": task.receipt.as_ref().map(|receipt| bounded_receipt_projection_with_proof_limit(receipt, 0)),
        "proof_references": proof_references,
        "workspace": {
            "epoch": task.workspace_status.epoch,
            "current_epoch": task.workspace_status.epoch,
            "snapshot_epoch": capsule.as_ref().map(|capsule| capsule.workspace_epoch),
            "snapshot_manifest_hash": capsule.as_ref().map(|capsule| capsule.workspace_manifest_hash.as_str()).filter(|value| value.len() <= MAX_PROJECTED_EXACT_STRING_BYTES),
            "last_progress_at": task.workspace_status.last_progress_at,
            "lease_state": task.workspace_status.lease_state,
            "next_required_action": bounded_prose(task.workspace_status.next_required_action.as_deref()),
        },
        "omitted_item_counts": {
            "observations": omitted_observations,
            "acceptance_criteria": omitted_acceptance_criteria,
            "unresolved_acceptance_criteria": omitted_unresolved_criteria,
            "required_evidence": omitted_required_evidence,
            "unresolved_required_evidence": omitted_unresolved_required_evidence,
            "read_scope": task.assignment.read_scope.len(),
            "write_scope": task.assignment.write_scope.len(),
            "proof_references": omitted_validation_calls,
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
        let passed = task.receipt.as_ref().is_some_and(|receipt| {
            receipt.criterion_results.iter().any(|result| {
                result.criterion_id == criterion.id && result.status == CriterionStatus::Passed
            })
        });
        if !passed {
            let inline_eligible = criterion.id.len() <= MAX_PROJECTED_EXACT_STRING_BYTES;
            let retained_inline = inline_eligible
                && retained_unresolved_criteria < MAX_PROJECTED_OBLIGATIONS_PER_KIND;
            if retained_inline {
                retained_unresolved_criteria = retained_unresolved_criteria.saturating_add(1);
            }
            if !retained_inline || criterion.text.len() > MAX_PROJECTED_DETAIL_BYTES {
                push_predetermined_json_pointer(
                    &mut selectors,
                    format!("task:unresolved-criterion:{}", criterion.id),
                    format!("{criteria_pointer}/{index}"),
                );
            }
        }
    }
    let mut retained_unresolved_evidence = 0_usize;
    for (index, requirement) in task.assignment.required_evidence.iter().enumerate() {
        if !required_evidence_is_satisfied(task, requirement) {
            let retained_inline = retained_unresolved_evidence < MAX_PROJECTED_OBLIGATIONS_PER_KIND;
            retained_unresolved_evidence = retained_unresolved_evidence.saturating_add(1);
            if !retained_inline || requirement.len() > MAX_PROJECTED_DETAIL_BYTES {
                push_predetermined_json_pointer(
                    &mut selectors,
                    format!("task:required-evidence:{index}"),
                    format!("/task/assignment/required_evidence/{index}"),
                );
            }
        }
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

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        bounded_task_projection_metadata(
            self,
            task_projection(self),
            task_projection_json_pointers(self),
        )
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<codex_tools::CanonicalToolResult> {
        serde_json::to_value(self)
            .ok()
            .map(codex_tools::CanonicalToolResult::json)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), GET_AGENT_TASK_TOOL)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
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
        "Read a durable typed-agent assignment, its current attempt, gates, receipt, captured \
         validation calls, and recent observations. A bound reviewer or verifier also receives \
         isolated evidence for its target. observation_limit defaults to 20 and cannot exceed 100.",
        object_schema(
            [
                (
                    "assignment_id",
                    JsonSchema::string(Some(
                        "UUIDv7 assignment identifier returned by spawn_agent.".to_string(),
                    )),
                ),
                (
                    "observation_limit",
                    JsonSchema::integer(Some(
                        "Number of newest observations to return, from 0 through 100. Defaults to \
                         20."
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
                string_array_schema("Canonical worker evidence requirements."),
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
                    "Concise evidence supporting the criterion result.".to_string(),
                )),
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
                        "Completed validation tool-call ids owned by this attempt.",
                    ),
                ),
                (
                    "blockers",
                    string_array_schema("Blockers that prevented completion."),
                ),
                (
                    "risks",
                    string_array_schema("Known remaining risks or uncertainties."),
                ),
                (
                    "next_action",
                    JsonSchema::string(Some(
                        "Recommended next action for the root agent.".to_string(),
                    )),
                ),
                ("architecture_contract", architecture_contract),
            ],
            &[
                "status",
                "summary",
                "criterion_results",
                "declared_changes",
                "validation_call_ids",
                "blockers",
                "risks",
            ],
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
            &["assignment_id", "gate", "status", "reason"],
        ),
    )
}

fn amend_agent_task_spec() -> ToolSpec {
    let criterion = object_schema(
        [
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
         after its review gate requests changes.",
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
         Risk, mutation, and ownership gates cannot be waived.",
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
         write claim.",
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

#[cfg(test)]
mod projection_tests {
    use super::*;
    use chrono::Utc;
    use codex_agent_task_store::Assignment;
    use codex_agent_task_store::AttemptId;
    use codex_agent_task_store::AttemptState;
    use codex_agent_task_store::CapabilityProfile;
    use codex_agent_task_store::LeaseState;
    use codex_agent_task_store::MutationEventId;
    use codex_agent_task_store::ObservationKind;
    use codex_agent_task_store::RuntimeObservation;
    use codex_agent_task_store::ValidationEvidence;
    use codex_agent_task_store::WakeEventId;
    use codex_agent_task_store::WorkspaceStrategy;
    use codex_agent_task_store::WorkspaceTaskStatus;

    #[test]
    fn oversized_receipt_projection_owner_recovery_is_hard_bounded_and_prioritized() {
        let receipt = AgentReceipt {
            assignment_id: AssignmentId::new(),
            attempt_id: AttemptId::new(),
            status: AgentStatusClaim::Completed,
            summary: "full durable receipt ".repeat(2_000),
            criterion_results: (0..100)
                .map(|index| CriterionResult {
                    criterion_id: format!("criterion-{index}"),
                    status: CriterionStatus::Passed,
                    evidence: Some("detailed durable criterion evidence ".repeat(20)),
                })
                .collect(),
            declared_changes: (0..100)
                .map(|index| DeclaredChange {
                    path: format!("src/{index:03}-{}.rs", "bounded-path-component".repeat(5)),
                    summary: "detailed durable change summary ".repeat(20),
                })
                .collect(),
            validation_call_ids: (0..100)
                .map(|index| format!("validation-{index:03}-{}", "proof".repeat(20)))
                .collect(),
            blockers: vec!["durable blocker detail ".repeat(1_000)],
            risks: vec!["durable risk detail ".repeat(1_000)],
            next_action: Some("durable next action ".repeat(1_000)),
            architecture_contract: None,
            evidence_epoch: 7,
            evidence_manifest_hash: "manifest".repeat(8),
            sealed_at: Utc::now(),
        };
        let result = SubmitAgentReceiptResult {
            receipt: receipt.clone(),
        };
        let metadata = bounded_task_projection_metadata(
            &result,
            bounded_receipt_projection(&result.receipt),
            receipt_projection_json_pointers(&result.receipt),
        )
        .expect("bounded projection metadata");

        assert!(
            serde_json::to_vec(&metadata.essential_inline)
                .expect("inline projection serializes")
                .len()
                <= TASK_OUTPUT_INLINE_LIMIT_BYTES
        );
        assert_eq!(
            metadata.essential_inline["assignment_id"],
            serde_json::to_value(receipt.assignment_id).expect("assignment id serializes")
        );
        assert_eq!(metadata.essential_inline["criterion_counts"]["total"], 100);
        assert_eq!(metadata.essential_inline["evidence_epoch"], 7);
        assert_eq!(
            metadata.essential_inline["evidence_manifest_hash"],
            receipt.evidence_manifest_hash
        );
        assert_eq!(
            metadata.essential_inline["sealed_at"],
            serde_json::to_value(receipt.sealed_at).expect("sealed timestamp serializes")
        );
        assert_eq!(
            metadata.essential_inline["omitted_item_counts"]["changed_paths"],
            92
        );
        assert_eq!(
            metadata.essential_inline["omitted_item_counts"]["proof_references"],
            92
        );
        assert!(
            metadata.spillable_text[0].contains("full durable receipt"),
            "the spill retains the complete canonical receipt"
        );
        assert_eq!(
            metadata.predetermined_json_pointers.len(),
            MAX_PREDETERMINED_JSON_POINTERS
        );
        assert_eq!(
            metadata.predetermined_json_pointers[0].pointer,
            "/receipt/blockers/0"
        );
        assert_eq!(
            metadata.predetermined_json_pointers[1].pointer,
            "/receipt/risks/0"
        );
        assert!(
            metadata
                .predetermined_json_pointers
                .iter()
                .all(|selector| !selector.pointer.starts_with("/receipt/criterion_results/")),
            "passed criterion evidence stays summarized"
        );
        let canonical = result
            .canonical_result(&ToolPayload::Function {
                arguments: "{}".to_string(),
            })
            .expect("canonical receipt JSON");
        assert_eq!(canonical.kind, codex_tools::CanonicalToolResultKind::Json);
        assert!(
            metadata
                .predetermined_json_pointers
                .iter()
                .all(|selector| canonical.json_pointers.contains_key(&selector.pointer))
        );
    }

    #[test]
    fn agent_snapshot_projection_owner_recovery_prioritizes_actionable_exact_fields() {
        let assignment_id = AssignmentId::new();
        let attempt_id = AttemptId::new();
        let now = Utc::now();
        let criteria = vec![
            AcceptanceCriterion {
                id: "criterion-closed".to_string(),
                text: "closed criterion".to_string(),
            },
            AcceptanceCriterion {
                id: "criterion-open".to_string(),
                text: "open criterion".to_string(),
            },
        ];
        let capsule = TaskCapsuleV1 {
            schema_version: 1,
            assignment_id,
            attempt_id,
            role: AgentRole::Worker,
            capability_profile: CapabilityProfile::ScopedSourceWrite,
            requirements: criteria.clone(),
            objective: "full canonical objective".to_string(),
            read_scope: Vec::new(),
            write_scope: Vec::new(),
            stop_condition: String::new(),
            dependencies: Vec::new(),
            risk_hints: Vec::new(),
            contract_claims: Vec::new(),
            workspace_strategy: None,
            relation: None,
            architecture_contract_ref: None,
            integration_plan: codex_agent_task_store::IntegrationPlan::SingleWriter,
            relevant_handles: Vec::new(),
            workspace_epoch: 11,
            workspace_manifest_hash: "a".repeat(64),
            prohibited_changes: Vec::new(),
            required_evidence: vec![
                "focused validation one".to_string(),
                "focused validation two".to_string(),
            ],
        };
        let observations = (0..6)
            .map(|index| RuntimeObservation {
                event_id: MutationEventId::new(),
                wake_event_id: WakeEventId::new(),
                assignment_id,
                attempt_id,
                kind: ObservationKind::Reading,
                summary: format!("observation-{index}"),
                call_id: Some(format!("call-{index}")),
                created_at: now,
            })
            .collect::<Vec<_>>();
        let first_retained_event = observations[2].event_id;
        let receipt = AgentReceipt {
            assignment_id,
            attempt_id,
            status: AgentStatusClaim::NeedsMain,
            summary: "terminal receipt".to_string(),
            criterion_results: vec![
                CriterionResult {
                    criterion_id: "criterion-closed".to_string(),
                    status: CriterionStatus::Passed,
                    evidence: Some("proof".to_string()),
                },
                CriterionResult {
                    criterion_id: "criterion-open".to_string(),
                    status: CriterionStatus::NotRun,
                    evidence: None,
                },
            ],
            declared_changes: Vec::new(),
            validation_call_ids: vec!["validation-satisfied".to_string()],
            blockers: Vec::new(),
            risks: Vec::new(),
            next_action: Some("finish the open criterion".to_string()),
            architecture_contract: None,
            evidence_epoch: 13,
            evidence_manifest_hash: "b".repeat(64),
            sealed_at: now,
        };
        let result = GetAgentTaskResult {
            task: AgentTask {
                assignment: Assignment {
                    assignment_id,
                    root_session_id: "root-session".to_string(),
                    admission_origin: codex_agent_task_store::AssignmentAdmissionOrigin::Typed,
                    repository_id: "repository".to_string(),
                    workspace_id: "workspace".to_string(),
                    role: AgentRole::Worker,
                    capability_profile: CapabilityProfile::ScopedSourceWrite,
                    objective: "full canonical objective".to_string(),
                    acceptance_criteria: criteria,
                    read_scope: Vec::new(),
                    write_scope: Vec::new(),
                    stop_condition: "done".to_string(),
                    dependencies: Vec::new(),
                    risk_hints: Vec::new(),
                    required_evidence: vec![
                        "focused validation one".to_string(),
                        "focused validation two".to_string(),
                    ],
                    prohibited_changes: Vec::new(),
                    contract_claims: Vec::new(),
                    workspace_strategy: WorkspaceStrategy::Shared,
                    start_epoch: 11,
                    relation: None,
                    architecture_contract_ref: None,
                    integration_plan: codex_agent_task_store::IntegrationPlan::SingleWriter,
                    task_capsule: Some(
                        serde_json::to_string(&capsule).expect("task capsule serializes"),
                    ),
                    created_at: now,
                },
                current_attempt: Attempt {
                    attempt_id,
                    assignment_id,
                    ordinal: 2,
                    amendment: None,
                    state: AttemptState::NeedsMain,
                    created_at: now,
                    sealed_at: Some(now),
                },
                gates: vec![
                    AgentGate {
                        assignment_id,
                        kind: GateKind::Review,
                        status: GateStatus::Pending,
                        reason: "review remains open".to_string(),
                        waiver_reason: None,
                        evidence_epoch: 12,
                        updated_at: now,
                        sealed_at: None,
                    },
                    AgentGate {
                        assignment_id,
                        kind: GateKind::Verification,
                        status: GateStatus::Passed,
                        reason: "verification passed".to_string(),
                        waiver_reason: None,
                        evidence_epoch: 12,
                        updated_at: now,
                        sealed_at: Some(now),
                    },
                ],
                receipt: Some(receipt),
                validation_calls: vec![ValidationCall {
                    call_id: "validation-satisfied".to_string(),
                    attempt_id,
                    command_summary: "focused validation one".to_string(),
                    resolved_executable: Some(r"C:\focused-validation.exe".to_string()),
                    proof_kind: ValidationProofKind::Focused,
                    evidence: ValidationEvidence {
                        end_epoch: Some(13),
                        ..ValidationEvidence::default()
                    },
                    status: ValidationCallStatus::Succeeded,
                    recorded_at: now,
                }],
                workspace_status: WorkspaceTaskStatus {
                    epoch: 14,
                    last_progress_at: Some(now),
                    lease_state: Some(LeaseState::Active),
                    pending_gates: vec![GateKind::Review],
                    stale_reason: None,
                    next_required_action: Some("resolve review".to_string()),
                    nudge_sent_at: None,
                },
                isolation_handoff: None,
                integration_handoffs: Vec::new(),
                observations,
            },
            cold_review_context: None,
        };
        let metadata = bounded_task_projection_metadata(
            &result,
            task_projection(&result),
            task_projection_json_pointers(&result),
        )
        .expect("bounded projection metadata");
        let projection = &metadata.essential_inline;

        assert!(
            serde_json::to_vec(projection)
                .expect("inline projection serializes")
                .len()
                <= TASK_OUTPUT_INLINE_LIMIT_BYTES
        );
        assert_eq!(projection["workspace"]["epoch"], 14);
        assert_eq!(projection["workspace"]["snapshot_epoch"], 11);
        assert_eq!(
            projection["workspace"]["snapshot_manifest_hash"],
            "a".repeat(64)
        );
        assert_eq!(projection["receipt"]["evidence_epoch"], 13);
        assert_eq!(
            projection["receipt"]["evidence_manifest_hash"],
            "b".repeat(64)
        );
        assert_eq!(projection["unresolved_gates"].as_array().unwrap().len(), 1);
        assert_eq!(projection["unresolved_gates"][0]["kind"], "review");
        assert_eq!(
            projection["unresolved_obligations"]["acceptance_criteria"][0]["id"],
            "criterion-open"
        );
        assert_eq!(
            projection["unresolved_obligations"]["required_evidence"][0]["ordinal"],
            2
        );
        assert_eq!(
            projection["recent_observations"].as_array().unwrap().len(),
            4
        );
        assert_eq!(
            projection["recent_observations"][0]["event_id"],
            serde_json::to_value(first_retained_event).expect("event id serializes")
        );
        assert_eq!(projection["omitted_item_counts"]["observations"], 2);
        assert!(
            metadata.spillable_text[0].contains("full canonical objective"),
            "the spill retains the full structured task snapshot"
        );
        let pointers = metadata
            .predetermined_json_pointers
            .iter()
            .map(|selector| selector.pointer.as_str())
            .collect::<BTreeSet<_>>();
        for expected in [
            "/task/gates/0",
            "/task/assignment/objective",
            "/task/validation_calls/0",
            "/task/receipt/criterion_results/1",
        ] {
            assert!(pointers.contains(expected), "missing {expected}");
        }
        assert!(!pointers.contains("/task/assignment/acceptance_criteria/0"));
        assert!(!pointers.contains("/task/assignment/acceptance_criteria/1"));
        assert!(!pointers.contains("/task/assignment/required_evidence/1"));
        assert!(!pointers.contains("/task/workspace_status/next_required_action"));
        assert!(!pointers.contains("/task/observations/0"));
        assert!(!pointers.contains("/task/observations/1"));
        let canonical = result
            .canonical_result(&ToolPayload::Function {
                arguments: "{}".to_string(),
            })
            .expect("canonical task JSON");
        assert_eq!(canonical.kind, codex_tools::CanonicalToolResultKind::Json);
        assert!(
            metadata
                .predetermined_json_pointers
                .iter()
                .all(|selector| canonical.json_pointers.contains_key(&selector.pointer))
        );
    }
}
