use super::*;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_agent_task_store::NonproductiveRecovery;
use codex_agent_task_store::WakeEventId;
use codex_agent_task_store::WakeRead;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::ToolSpec;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
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
            cancellation_token,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        let timeout_ms = match args.timeout_ms {
            Some(ms) if ms < min_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(
                    "Omit timeout_ms for the normal wait. Use list_agents or get_agent_task for an immediate status snapshot.".to_owned(),
                ));
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

        let explicit_cursor = args.cursor.is_some();
        let parsed_cursor = args
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
        let consuming_agent_path = turn
            .session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root)
            .to_string();
        let mut cursor = match (explicit_cursor, store.as_ref(), root_session_id.as_deref()) {
            (false, Some(store), Some(root_session_id)) => store
                .automatic_wake_cursor(root_session_id.to_string(), consuming_agent_path.clone())
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "wait_agent could not initialize its automatic cursor: {error}"
                    ))
                })?,
            _ => parsed_cursor,
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let wait_started = Instant::now();
        let mut pending_activity = pending_activity;
        let mut unchanged_store_polls = 0_u32;
        let (outcome, wake_read) = loop {
            let wait = wait_for_activity(
                &mut activity_rx,
                pending_activity.take(),
                deadline,
                store.as_ref(),
                root_session_id.as_deref(),
                cursor,
            );
            let (outcome, wake_read, unchanged_polls) = tokio::select! {
                result = wait => result,
                _ = cancellation_token.cancelled() => {
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
            }
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "wait_agent could not read durable typed-task progress: {error}"
                ))
            })?;
            unchanged_store_polls = unchanged_store_polls.saturating_add(unchanged_polls);
            let (Some(store), Some(root_session_id), Some(next_cursor)) = (
                store.as_ref(),
                root_session_id.as_deref(),
                wake_read.latest_event_id,
            ) else {
                break (outcome, wake_read);
            };
            if explicit_cursor || wake_read.updated_agents.is_empty() {
                break (outcome, wake_read);
            }
            if store
                .compare_and_swap_automatic_wake_cursor(
                    root_session_id.to_string(),
                    consuming_agent_path.clone(),
                    cursor,
                    next_cursor,
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "wait_agent could not advance its automatic cursor: {error}"
                    ))
                })?
            {
                break (outcome, wake_read);
            }

            cursor = store
                .automatic_wake_cursor(root_session_id.to_string(), consuming_agent_path.clone())
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "wait_agent could not reread its automatic cursor: {error}"
                    ))
                })?;
            if outcome != WaitOutcome::DurableActivity {
                break (
                    outcome,
                    WakeRead {
                        reason: None,
                        updated_agents: Vec::new(),
                        latest_event_id: cursor,
                        truncated_count: 0,
                        timed_out: false,
                    },
                );
            }
        };
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
            coordinator.record_first_meaningful_progress_once(
                event.attempt_id,
                event.reason,
                &turn.session_telemetry,
            );
            if task.receipt.is_some() {
                coordinator
                    .record_root_receipt_hydration_once(event.attempt_id, &turn.session_telemetry);
            }
            typed_deltas.push(json!({
                "event_id": event.event_id,
                "assignment_id": event.assignment_id,
                "attempt_id": event.attempt_id,
                "reason": event.reason,
                "summary": event.summary,
                "created_at": event.created_at,
                "epoch": task.workspace_status.epoch,
                "gates": task.gates,
                "receipt": durable_receipt_pointer(
                    event.assignment_id.to_string(),
                    task.receipt.is_some(),
                ),
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
        let mut result = WaitAgentResult::from_outcome(
            outcome,
            wake_read
                .latest_event_id
                .map(|event_id| event_id.to_string()),
            typed_deltas,
            wake_read.truncated_count,
            nudged_assignment_ids,
        );
        if unchanged_store_polls > 0 {
            let resource_identity_hash = sha256_text(&format!(
                "agent-event-wait\0{}\0{}",
                root_session_id.as_deref().unwrap_or_default(),
                consuming_agent_path,
            ));
            let state_revision = sha256_text(
                &json!({
                    "cursor": &result.cursor,
                    "typed_deltas": &result.typed_deltas,
                    "timed_out": result.timed_out,
                })
                .to_string(),
            );
            result.deterministic_continuation_receipts.push(
                TurnTimingDeterministicContinuationReceipt {
                    class: DeterministicContinuationClass::AgentEventWait,
                    resource_identity_hash,
                    state_revision,
                    host_action: DeterministicContinuationHostAction::AwaitStateChange,
                    suppressed_continuation_count: unchanged_store_polls,
                    avoided_token_usage: None,
                },
            );
        }
        turn.session_telemetry.counter(
            "codex.multi_agent.root_wait",
            1,
            &[(
                "outcome",
                match outcome {
                    WaitOutcome::MailboxActivity => "mailbox",
                    WaitOutcome::DurableActivity => "durable_progress",
                    WaitOutcome::Steered => "steered",
                    WaitOutcome::TimedOut => "timed_out",
                },
            )],
        );
        turn.session_telemetry.histogram(
            "codex.multi_agent.root_wait_duration_ms",
            i64::try_from(wait_started.elapsed().as_millis()).unwrap_or(i64::MAX),
            &[],
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

fn durable_receipt_pointer(assignment_id: String, available: bool) -> JsonValue {
    json!({
        "available": available,
        "source": "get_agent_task",
        "assignment_id": assignment_id,
    })
}

fn sha256_text(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_receipt_pointer_never_embeds_receipt_contents() {
        let pointer = durable_receipt_pointer("assignment-1".to_string(), true);

        assert_eq!(
            pointer,
            json!({
                "available": true,
                "source": "get_agent_task",
                "assignment_id": "assignment-1",
            })
        );
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
    #[serde(skip)]
    pub(crate) deterministic_continuation_receipts: Vec<TurnTimingDeterministicContinuationReceipt>,
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
            deterministic_continuation_receipts: Vec::new(),
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
        match store
            .recover_nonproductive_assignment(binding.assignment_id, no_progress_before)
            .await
        {
            Ok(NonproductiveRecovery::Recovered { productivity, .. }) => {
                let _ = session
                    .services
                    .agent_control
                    .interrupt_agent(thread_id)
                    .await;
                if productivity.cancelled_expired_operation_count > 0 {
                    turn.session_telemetry.counter(
                        "codex.multi_agent.bounded_operation",
                        i64::from(productivity.cancelled_expired_operation_count),
                        &[("outcome", "cancelled_at_deadline")],
                    );
                }
                turn.session_telemetry.counter(
                    "codex.multi_agent.nonproductive_recovery",
                    1,
                    &[("outcome", "abandoned")],
                );
                continue;
            }
            Ok(NonproductiveRecovery::Suspended(productivity)) => {
                turn.session_telemetry.counter(
                    "codex.multi_agent.bounded_operation",
                    i64::from(productivity.active_owned_operation_count),
                    &[("outcome", "suspended_recovery")],
                );
                continue;
            }
            Ok(NonproductiveRecovery::NotEligible) => {}
            Err(error) => {
                tracing::warn!(
                    %error,
                    assignment_id = %binding.assignment_id,
                    "failed to evaluate nonproductive typed assignment"
                );
                continue;
            }
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

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(self, true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.deterministic_continuation_receipts.clone()
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
) -> codex_agent_task_store::StoreResult<(WaitOutcome, WakeRead, u32)> {
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
            0,
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
            0,
        ));
    }
    let initial = read_wakes().await?;
    if !initial.updated_agents.is_empty() {
        return Ok((WaitOutcome::DurableActivity, initial, 0));
    }
    let mut unchanged_polls = 1_u32;
    loop {
        let poll_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_millis(250));
        match timeout_at(poll_deadline, activity_rx.changed()).await {
            Ok(Ok(())) => {
                let outcome = match *activity_rx.borrow_and_update() {
                    InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
                    InputQueueActivity::Steer => WaitOutcome::Steered,
                };
                return Ok((outcome, read_wakes().await?, unchanged_polls));
            }
            Ok(Err(_)) => {
                return Ok((WaitOutcome::TimedOut, read_wakes().await?, unchanged_polls));
            }
            Err(_) => {
                let durable = read_wakes().await?;
                if !durable.updated_agents.is_empty() {
                    return Ok((WaitOutcome::DurableActivity, durable, unchanged_polls));
                }
                unchanged_polls = unchanged_polls.saturating_add(1);
                if Instant::now() >= deadline {
                    return Ok((WaitOutcome::TimedOut, durable, unchanged_polls));
                }
            }
        }
    }
}
