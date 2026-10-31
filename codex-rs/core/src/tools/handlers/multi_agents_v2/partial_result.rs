//! Nonterminal evidence uses the task store, not ancestor progress mail or a
//! worker-private artifact. Reads retain the existing team/reviewer boundary.

use super::*;
use codex_agent_task_store::AgentPartialResult;
use codex_agent_task_store::PartialResultDraft;

const PUBLISH_TOOL: &str = "publish_agent_partial_result";
const GET_TOOL: &str = "get_agent_partial_result";

pub(crate) struct PublishAgentPartialResultHandler;
pub(crate) struct GetAgentPartialResultHandler;
pub(crate) struct GroupAgentFindingsHandler;

define_handler!(
    PublishAgentPartialResultHandler,
    PUBLISH_TOOL,
    publish_spec,
    publish,
    false
);
define_handler!(GetAgentPartialResultHandler, GET_TOOL, get_spec, get, true);
define_handler!(GroupAgentFindingsHandler, "group_agent_findings", group_spec, group, true);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingReportRef {
    assignment_id: AssignmentId,
    attempt_id: AttemptId,
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupArgs {
    results: Vec<FindingReportRef>,
}

async fn group(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel("group_agent_findings requires function arguments".into()));
    };
    let args: GroupArgs = parse_arguments(arguments)?;
    if args.results.is_empty() {
        return Err(FunctionCallError::RespondToModel("finding report selection cannot be empty".into()));
    }
    let coordinator = invocation.session.services.agent_control.task_coordinator();
    let store = coordinator.store().ok_or_else(||
        FunctionCallError::RespondToModel("finding report store unavailable".into()))?;
    let mut seen = BTreeSet::new();
    let mut reports = Vec::new();
    for reference in args.results {
        if !seen.insert((reference.assignment_id, reference.attempt_id, reference.version)) {
            return Err(FunctionCallError::RespondToModel("duplicate report reference; no occurrence counts were combined".into()));
        }
        let task = load_authorized_task(coordinator, &invocation.step_context.turn,
            reference.assignment_id, 0, "group_agent_findings").await?;
        let result = store.get_agent_partial_result(task.assignment.root_session_id,
            reference.assignment_id, Some(reference.attempt_id), Some(reference.version)).await
            .map_err(|error| task_store_error("group_agent_findings", error))?
            .ok_or_else(|| FunctionCallError::RespondToModel("selected immutable finding report is unavailable; selection was not silently omitted".into()))?;
        reports.push(result);
    }
    // Equality is deliberately conservative. Similar prose, common paths, or a
    // common defect alone are never a causal-equivalence proof. Different source
    // hashes, locations, report evidence or limitations keep separate groups and
    // verification obligations, even when the finding's own fields match.
    // Serialize shared report context once, not once per finding. Keys borrow
    // these immutable contexts and findings; original reports remain untouched.
    let report_contexts = reports.iter().map(|report| {
        serde_json::to_string(&(
            &report.repository_id, &report.workspace_id, &report.result.sources,
            &report.result.evidence, &report.result.limitations,
        ))
    }).collect::<Result<Vec<_>, _>>()
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let mut exact = std::collections::BTreeMap::new();
    let mut candidates = std::collections::BTreeMap::<String, Vec<usize>>::new();
    let mut groups = Vec::<JsonValue>::new();
    for (report_index, report) in reports.iter().enumerate() {
        for (finding_index, finding) in report.result.findings.iter().enumerate() {
            let key = (
                &report_contexts[report_index],
                &finding.defect, &finding.trigger, &finding.remedy,
                &finding.locations, &finding.evidence,
            );
            let index = if let Some(index) = exact.get(&key) { *index } else {
                let index = groups.len();
                exact.insert(key, index);
                groups.push(json!({"kind":"exact_report_equivalence", "members":[], "reported_occurrence_count":0_u64}));
                let candidate = serde_json::to_string(&(&finding.defect, &finding.trigger, &finding.remedy))
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                candidates.entry(candidate).or_default().push(index);
                index
            };
            let count = groups[index]["reported_occurrence_count"].as_u64().unwrap_or(0)
                .checked_add(finding.occurrence_count).ok_or_else(||
                    FunctionCallError::RespondToModel("reported occurrence count overflow; original reports remain retained".into()))?;
            groups[index]["reported_occurrence_count"] = json!(count);
            groups[index]["members"].as_array_mut().ok_or_else(||
                FunctionCallError::RespondToModel("invalid finding group".into()))?
                .push(json!({"report_index":report_index, "finding_index":finding_index, "original_id":finding.id}));
        }
    }
    let verification_candidates = candidates.into_values().filter(|indices| indices.len() > 1)
        .map(|indices| json!({"groups":indices, "reason":"matching defect/trigger/remedy but distinct locations, repository/workspace, source identities, evidence or limitations; verify every occurrence separately"}))
        .collect::<Vec<_>>();
    let output = codex_tools::JsonToolOutput::new(json!({
        "reports":reports, "groups":groups, "verification_candidates":verification_candidates,
        "acceptance":"not_evaluated", "discards_reports":false,
        "count_semantics":"sum of original reported counts, not unique physical defects",
    }));
    crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
    Ok(boxed_tool_output(output))
}

fn group_spec() -> ToolSpec {
    function_spec("group_agent_findings",
        "Losslessly group findings from exact immutable worker partial-result versions. Only identical defect, trigger, remedy, locations, repository/workspace, source identities/hashes, finding and report evidence, and report limitations form a group; each original report, finding ID, location, evidence and occurrence count remains in reports, indexed by group members. Counts sum reported occurrences, not unique physical defects. Matching defect/trigger/remedy with distinct evidence, locations or limitations remains a verification candidate, not permission to discard a report or skip any occurrence. Provisional claims are not independently verified by grouping. Missing, unauthorized or duplicate report selections fail the whole call instead of silently narrowing coverage. Reports without structured findings remain intact. This read-only tool does not change acceptance, receipts, gates or work obligations.",
        object_schema([("results", JsonSchema::array(object_schema([
            ("assignment_id", assignment_id_schema()),
            ("attempt_id", JsonSchema::string(None)),
            ("version", JsonSchema::integer(Some("Exact immutable partial-result version, starting at 1.".into()))),
        ], &["assignment_id", "attempt_id", "version"]), Some("Nonempty exact immutable worker report selections; none may be omitted on failure.".into())))], &["results"]),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishArgs {
    expected_version: u32,
    result: PartialResultDraft,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArgs {
    assignment_id: String,
    attempt_id: Option<String>,
    version: Option<u32>,
}

#[derive(Serialize)]
struct PartialResultOutput {
    partial_result: Option<AgentPartialResult>,
    acceptance: &'static str,
    completes_assignment: bool,
    retrieval: JsonValue,
}

// Use the existing complete JSON retention/projection path for both direct
// and Code Mode calls. Parents acquire their own recoverable artifact on read.
impl ToolOutput for PartialResultOutput {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, GET_TOOL)
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(self, true)
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<codex_tools::CanonicalToolResult> {
        serde_json::to_value(self)
            .ok()
            .map(codex_tools::CanonicalToolResult::json)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), GET_TOOL)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, GET_TOOL)
    }
}

fn output(assignment_id: AssignmentId, result: Option<AgentPartialResult>) -> PartialResultOutput {
    let retrieval = match &result {
        Some(result) => json!({
            "tool": GET_TOOL,
            "arguments": {
                "assignment_id": assignment_id,
                "attempt_id": result.attempt_id,
                "version": result.version,
            },
        }),
        None => json!({"tool": GET_TOOL, "arguments": {"assignment_id": assignment_id}}),
    };
    PartialResultOutput {
        partial_result: result,
        acceptance: "not_evaluated",
        completes_assignment: false,
        retrieval,
    }
}

async fn publish(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        ..
    } = invocation;
    let args: PublishArgs = parse_arguments(&function_arguments(payload)?)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let binding = coordinator
        .binding_for_turn(&step_context.turn)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "{PUBLISH_TOOL}: caller has no bound assignment"
            ))
        })?;
    // The store checks active/current ownership and team identity atomically
    // with publication. Never retarget a stale binding to a correction attempt.
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!("{PUBLISH_TOOL}: task store is unavailable"))
    })?;
    let result = store
        .publish_agent_partial_result(
            binding.root_session_id,
            binding.attempt_id,
            args.expected_version,
            args.result,
        )
        .await
        .map_err(|error| task_store_error(PUBLISH_TOOL, error))?;
    Ok(boxed_tool_output(output(
        binding.assignment_id,
        Some(result),
    )))
}

async fn get(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        ..
    } = invocation;
    let args: GetArgs = parse_arguments(&function_arguments(payload)?)?;
    let assignment_id = parse_assignment_id(GET_TOOL, &args.assignment_id)?;
    let coordinator = session.services.agent_control.task_coordinator();
    let task =
        load_authorized_task(coordinator, &step_context.turn, assignment_id, 0, GET_TOOL).await?;
    let attempt_id = args
        .attempt_id
        .as_deref()
        .map(AttemptId::parse)
        .transpose()
        .map_err(|error| task_store_error(GET_TOOL, error))?;
    let store = coordinator.store().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!("{GET_TOOL}: task store is unavailable"))
    })?;
    let result = store
        .get_agent_partial_result(
            task.assignment.root_session_id,
            assignment_id,
            attempt_id,
            args.version,
        )
        .await
        .map_err(|error| task_store_error(GET_TOOL, error))?;
    Ok(boxed_tool_output(output(assignment_id, result)))
}

fn publish_spec() -> ToolSpec {
    function_spec(
        PUBLISH_TOOL,
        "Publish immutable, versioned PROVISIONAL evidence for your active bound attempt without ending the assignment. Supply expected_version=0 initially, then the latest retained version; conflicting publications are rejected and an identical immediate retry is idempotent. Copy actual intermediate evidence into result.evidence (not merely a worker-private artifact pointer), preserve source identities/hashes and limitations. Source declarations are not host-verified acquisition or freshness proof. The full result JSON is limited to 64 KiB. Publication produces a task wake observation, not ancestor progress mail, accepted deliverables, a sealed receipt or terminal completion. The parent retrieves it through get_agent_partial_result and independently evaluates it; final receipt/completion workflows are unchanged.",
        object_schema([
            ("expected_version", JsonSchema::integer(Some("Latest retained version for this attempt, or 0 for its first publication.".into()))),
            ("result", object_schema([
                ("summary", JsonSchema::string(Some("Nonempty intermediate-result summary.".into()))),
                ("evidence", JsonSchema::string(Some("Nonempty self-contained evidence copied into shared task storage. Private references alone are insufficient.".into()))),
                ("sources", JsonSchema::array(object_schema([
                    ("identity", JsonSchema::string(Some("Exact source path/revision or immutable evidence identity.".into()))),
                    ("sha256", JsonSchema::string(Some("64 hexadecimal digits identifying the reported source bytes.".into()))),
                ], &["identity", "sha256"]), Some("1-128 worker-declared source identities; retain original hashes.".into()))),
                ("limitations", string_array_schema("Explicit limitations and unverified portions; use [] only when none are known.")),
                ("findings", JsonSchema::array(object_schema([
                    ("id", JsonSchema::string(Some("Original finding ID; unique within this report.".into()))),
                    ("defect", JsonSchema::string(None)),
                    ("trigger", JsonSchema::string(None)),
                    ("remedy", JsonSchema::string(None)),
                    ("locations", string_array_schema("All original locations; nonempty.")),
                    ("occurrence_count", JsonSchema::integer(Some("Positive original reported count; never infer unique-defect count.".into()))),
                    ("evidence", string_array_schema("All distinct original evidence, not only a representative; nonempty.")),
                ], &["id", "defect", "trigger", "remedy", "locations", "occurrence_count", "evidence"]), Some("Optional structured findings for lossless group_agent_findings. Uncertain overlap must retain separate occurrences.".into()))),
            ], &["summary", "evidence", "sources", "limitations"])),
        ], &["expected_version", "result"]),
    )
}

fn get_spec() -> ToolSpec {
    function_spec(
        GET_TOOL,
        "Retrieve a worker's durable PROVISIONAL partial result through the existing team task owner. Omit attempt_id/version for the current attempt's latest result, or supply both to recover an exact immutable historical publication. Versions start at 1 and are scoped to an attempt. Null means no matching publication, not successful completion. Evidence, source identities and limitations are retained here, independently of worker-private artifacts; oversized delivery uses the caller's ordinary read_tool_output recovery. Partial results never establish acceptance or terminal completion. Team and independent-review read restrictions are identical to get_agent_task.",
        object_schema([
            ("assignment_id", assignment_id_schema()),
            ("attempt_id", JsonSchema::string(Some("Optional exact producer attempt; omitted selects the current attempt.".into()))),
            ("version", JsonSchema::integer(Some("Optional exact version (>=1); omitted selects the latest publication for the selected attempt.".into()))),
        ], &["assignment_id"]),
    )
}
