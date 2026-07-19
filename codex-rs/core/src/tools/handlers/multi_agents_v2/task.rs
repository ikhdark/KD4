use super::*;
use codex_agent_task_store::AcceptanceCriterion;
use codex_agent_task_store::AgentGate;
use codex_agent_task_store::AgentReceipt;
use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptAmendment;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::DEFAULT_OBSERVATION_LIMIT;
use codex_agent_task_store::DeclaredChange;
use codex_agent_task_store::GateKind;
use codex_agent_task_store::GateStatus;
use codex_agent_task_store::MAX_OBSERVATION_LIMIT;
use codex_agent_task_store::ReceiptDraft;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::TaskActor;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;

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
    Ok(boxed_tool_output(GetAgentTaskResult { task }))
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
    store
        .finalize_pending_mutations(binding.attempt_id)
        .await
        .map_err(|error| task_store_error(SUBMIT_AGENT_RECEIPT_TOOL, error))?;
    let receipt = store
        .submit_agent_receipt(binding.attempt_id, args.into_receipt_draft())
        .await
        .map_err(|error| task_store_error(SUBMIT_AGENT_RECEIPT_TOOL, error))?;
    Ok(boxed_tool_output(SubmitAgentReceiptResult { receipt }))
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
    let store = session
        .services
        .agent_control
        .task_coordinator()
        .store()
        .ok_or_else(|| {
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
}

#[derive(Debug, Serialize)]
struct SubmitAgentReceiptResult {
    receipt: AgentReceipt,
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

            fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
                tool_output_response_item(call_id, payload, self, Some(true), $tool_name)
            }

            fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
                tool_output_code_mode_result(self, $tool_name)
            }
        }
    };
}

impl_json_output!(GetAgentTaskResult, GET_AGENT_TASK_TOOL);
impl_json_output!(SubmitAgentReceiptResult, SUBMIT_AGENT_RECEIPT_TOOL);
impl_json_output!(SetAgentGateResult, SET_AGENT_GATE_TOOL);
impl_json_output!(AmendAgentTaskResult, AMEND_AGENT_TASK_TOOL);
impl_json_output!(WaiveAgentGateResult, WAIVE_AGENT_GATE_TOOL);
impl_json_output!(AbandonAgentTaskResult, ABANDON_AGENT_TASK_TOOL);

fn get_agent_task_spec() -> ToolSpec {
    function_spec(
        GET_AGENT_TASK_TOOL,
        "Read a durable typed-agent assignment, its current attempt, gates, receipt, captured \
         validation calls, and recent observations. observation_limit defaults to 20 and cannot \
         exceed 100.",
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
