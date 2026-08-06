use super::*;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_agent_task_store::WakeEventId;
use codex_agent_task_store::WakeRead;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        let timeout_ms = match args.timeout_ms {
            Some(ms) if ms < min_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "timeout_ms must be at least {min_timeout_ms}"
                )));
            }
            Some(ms) if ms > max_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "timeout_ms must be at most {max_timeout_ms}"
                )));
            }
            Some(ms) => ms,
            None => default_timeout_ms,
        };

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        let cursor = args
            .cursor
            .as_deref()
            .map(WakeEventId::parse)
            .transpose()
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "wait_agent cursor is invalid or no longer retained: {error}"
                ))
            })?;
        let coordinator = session.services.agent_control.task_coordinator();
        // Already-pending input does not need durable task-store state. Avoid
        // delaying immediate mailbox or steering delivery on store startup.
        if pending_activity.is_none() && coordinator.store().is_none() {
            coordinator
                .initialize_for_workspace_coordination(
                    session.services.state_db.clone(),
                    turn.config.sqlite_home.clone(),
                    turn.config.model_provider_id.clone(),
                    session.services.agent_control.session_id().to_string(),
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "wait_agent could not initialize durable typed-task progress: {error}"
                    ))
                })?;
        }
        let store = coordinator.store();
        let root_session_id = coordinator.root_session_id();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let (outcome, wake_read) = wait_for_activity(
            &mut activity_rx,
            pending_activity,
            deadline,
            store.as_ref(),
            root_session_id.as_deref(),
            cursor,
        )
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "wait_agent could not read durable typed-task progress: {error}"
            ))
        })?;
        let mut typed_deltas = Vec::with_capacity(wake_read.updated_agents.len());
        for event in &wake_read.updated_agents {
            let task = coordinator
                .get_agent_task(event.assignment_id, Some(0))
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "wait_agent could not hydrate assignment {}: {error}",
                        event.assignment_id
                    ))
                })?;
            typed_deltas.push(json!({
                "event_id": event.event_id,
                "assignment_id": event.assignment_id,
                "attempt_id": event.attempt_id,
                "reason": event.reason,
                "summary": event.summary,
                "created_at": event.created_at,
                "epoch": task.workspace_status.epoch,
                "gates": task.gates,
                "receipt": task.receipt,
                "last_progress_at": task.workspace_status.last_progress_at,
                "lease_state": task.workspace_status.lease_state,
                "stale_reason": task.workspace_status.stale_reason,
                "next_required_action": task.workspace_status.next_required_action,
                "nudge_sent_at": task.workspace_status.nudge_sent_at,
            }));
        }
        let nudged_assignment_ids =
            if outcome == WaitOutcome::TimedOut && !turn.session_source.is_non_root_agent() {
                nudge_stalled_assignments(
                    session.as_ref(),
                    turn.as_ref(),
                    store.as_ref(),
                    root_session_id.as_deref(),
                )
                .await
            } else {
                Vec::new()
            };
        let result = WaitAgentResult::from_outcome(
            outcome,
            wake_read
                .latest_event_id
                .map(|event_id| event_id.to_string()),
            typed_deltas,
            wake_read.truncated_count,
            nudged_assignment_ids,
        );

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<i64>,
    cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    pub(crate) cursor: Option<String>,
    pub(crate) typed_deltas: Vec<JsonValue>,
    pub(crate) truncated_count: u64,
    pub(crate) nudged_assignment_ids: Vec<String>,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: WaitOutcome,
        cursor: Option<String>,
        typed_deltas: Vec<JsonValue>,
        truncated_count: u64,
        nudged_assignment_ids: Vec<String>,
    ) -> Self {
        let mut message = match outcome {
            WaitOutcome::MailboxActivity => "Wait completed.",
            WaitOutcome::DurableActivity => "Durable typed-task progress is available.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::TimedOut => "Wait timed out.",
        }
        .to_string();
        if !nudged_assignment_ids.is_empty() {
            message.push_str(&format!(
                " Sent the one allowed no-progress nudge to assignments [{}].",
                nudged_assignment_ids.join(", ")
            ));
        }
        Self {
            message,
            timed_out: outcome == WaitOutcome::TimedOut,
            cursor,
            typed_deltas,
            truncated_count,
            nudged_assignment_ids,
        }
    }
}

async fn nudge_stalled_assignments(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    store: Option<&std::sync::Arc<dyn codex_agent_task_store::AgentTaskStore>>,
    root_session_id: Option<&str>,
) -> Vec<String> {
    let (Some(store), Some(root_session_id)) = (store, root_session_id) else {
        return Vec::new();
    };
    let author = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let Ok(bindings) = store
        .list_agent_task_bindings(root_session_id.to_string(), None)
        .await
    else {
        return Vec::new();
    };
    let no_progress_before = chrono::Utc::now()
        - chrono::Duration::seconds(codex_agent_task_store::DEFAULT_WORKSPACE_LEASE_SECONDS);
    let mut nudged = Vec::new();
    for binding in bindings {
        if !is_strict_agent_descendant(&author, &binding.agent_path) {
            continue;
        }
        let Some(thread_id) = binding
            .thread_id
            .as_deref()
            .and_then(|value| codex_protocol::ThreadId::from_string(value).ok())
        else {
            continue;
        };
        let status = session.services.agent_control.get_status(thread_id).await;
        if is_final(&status) {
            continue;
        }
        let Ok(true) = store
            .reserve_stalled_nudge(binding.assignment_id, no_progress_before)
            .await
        else {
            continue;
        };
        let Ok(receiver) = session.services.agent_control.ensure_agent_known(thread_id) else {
            let _ = store.release_stalled_nudge(binding.assignment_id).await;
            continue;
        };
        let Some(receiver_path) = receiver.agent_path else {
            let _ = store.release_stalled_nudge(binding.assignment_id).await;
            continue;
        };
        if !is_strict_agent_descendant(&author, receiver_path.as_ref()) {
            let _ = store.release_stalled_nudge(binding.assignment_id).await;
            continue;
        }
        let mut communication = communication_from_tool_message(
            author.clone(),
            receiver_path,
            "Coordination nudge: no durable progress has been observed. Report current progress or the concrete blocker; do not restart a stale validation loop."
                .to_string(),
        );
        communication.trigger_turn = true;
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Message, session.thread_id);
        if session
            .services
            .agent_control
            .send_inter_agent_communication(thread_id, communication, context)
            .await
            .is_ok()
        {
            nudged.push(binding.assignment_id.to_string());
        } else if let Err(error) = store.release_stalled_nudge(binding.assignment_id).await {
            tracing::warn!(
                %error,
                assignment_id = %binding.assignment_id,
                "failed to release an undelivered stalled-task nudge"
            );
        }
    }
    nudged
}

fn is_strict_agent_descendant(caller: &AgentPath, candidate: &str) -> bool {
    let mut prefix = caller.to_string();
    prefix.push('/');
    candidate.starts_with(&prefix)
}

impl ToolOutput for WaitAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    MailboxActivity,
    DurableActivity,
    Steered,
    TimedOut,
}

async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
    store: Option<&std::sync::Arc<dyn codex_agent_task_store::AgentTaskStore>>,
    root_session_id: Option<&str>,
    cursor: Option<WakeEventId>,
) -> codex_agent_task_store::StoreResult<(WaitOutcome, WakeRead)> {
    let read_wakes = || async {
        match (store, root_session_id) {
            (Some(store), Some(root_session_id)) => {
                store
                    .read_wake_events(root_session_id.to_string(), cursor)
                    .await
            }
            _ => Ok(WakeRead {
                reason: None,
                updated_agents: Vec::new(),
                latest_event_id: cursor,
                truncated_count: 0,
                timed_out: true,
            }),
        }
    };
    if let Some(activity) = pending_activity {
        let outcome = match activity {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        };
        return Ok((
            outcome,
            WakeRead {
                reason: None,
                updated_agents: Vec::new(),
                latest_event_id: cursor,
                truncated_count: 0,
                timed_out: false,
            },
        ));
    }
    if Instant::now() >= deadline {
        return Ok((
            WaitOutcome::TimedOut,
            WakeRead {
                reason: None,
                updated_agents: Vec::new(),
                latest_event_id: cursor,
                truncated_count: 0,
                timed_out: true,
            },
        ));
    }
    let initial = read_wakes().await?;
    if !initial.updated_agents.is_empty() {
        return Ok((WaitOutcome::DurableActivity, initial));
    }
    loop {
        let poll_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_millis(250));
        match timeout_at(poll_deadline, activity_rx.changed()).await {
            Ok(Ok(())) => {
                let outcome = match *activity_rx.borrow_and_update() {
                    InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
                    InputQueueActivity::Steer => WaitOutcome::Steered,
                };
                return Ok((outcome, read_wakes().await?));
            }
            Ok(Err(_)) => return Ok((WaitOutcome::TimedOut, read_wakes().await?)),
            Err(_) => {
                let durable = read_wakes().await?;
                if !durable.updated_agents.is_empty() {
                    return Ok((WaitOutcome::DurableActivity, durable));
                }
                if Instant::now() >= deadline {
                    return Ok((WaitOutcome::TimedOut, durable));
                }
            }
        }
    }
}
