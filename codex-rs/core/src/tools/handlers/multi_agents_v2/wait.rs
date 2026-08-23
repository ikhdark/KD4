use super::*;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::MAX_WAKE_EVENTS_PER_READ;
use codex_agent_task_store::MAX_WAKE_EVENTS_PER_ROOT;
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
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_WAKE_EVENT_DRAIN_PAGES: usize =
    MAX_WAKE_EVENTS_PER_ROOT.div_ceil(MAX_WAKE_EVENTS_PER_READ);

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
        let explicit_timeout_ms = match args.timeout_ms {
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
            Some(ms) => Some(ms),
            None => None,
        };
        let maintenance_interval =
            Duration::from_millis(u64::try_from(default_timeout_ms.max(0)).unwrap_or(u64::MAX));

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
        let wait_started = Instant::now();
        let deadline = explicit_timeout_ms
            .map(|timeout_ms| wait_started + Duration::from_millis(timeout_ms as u64));
        let mut maintenance_deadline = Some(wait_started + maintenance_interval);
        let mut pending_activity = pending_activity;
        let mut activity_open = true;
        let mut unchanged_store_polls = 0_u32;
        let mut drained_event_pages = 0_u32;
        let mut nudged_assignment_ids = Vec::new();
        let (outcome, wake_read, hydrated_assignments) = 'wait_owner: loop {
            if pending_activity.is_none() {
                let current = read_wake_events(store.as_ref(), root_session_id.as_deref(), cursor)
                    .await
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "wait_agent could not inspect durable progress before waiting: {error}"
                        ))
                    })?;
                if current.updated_agents.is_empty() {
                    let owners = hydrate_wait_owner_assignments(
                        coordinator,
                        store.as_ref(),
                        root_session_id.as_deref(),
                        &consuming_agent_path,
                    )
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                    if wait_owner_is_settled(&owners) {
                        break 'wait_owner (WaitOutcome::MaintenanceActivity, current, owners);
                    }
                }
            }
            let boundary_deadline = earliest_deadline(deadline, maintenance_deadline);
            let wait = wait_for_activity(
                &mut activity_rx,
                &mut activity_open,
                pending_activity.take(),
                boundary_deadline,
                store.as_ref(),
                root_session_id.as_deref(),
                cursor,
            );
            let (mut outcome, mut wake_read) = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    turn.turn_timing_state
                        .record_internally_drained_waits(drained_event_pages);
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
                result = wait => result,
            }
            .map_err(|error| {
                turn.turn_timing_state
                    .record_internally_drained_waits(drained_event_pages);
                FunctionCallError::RespondToModel(format!(
                    "wait_agent could not read durable typed-task progress: {error}"
                ))
            })?;

            if outcome == WaitOutcome::BoundaryElapsed {
                let now = Instant::now();
                let maintenance_due = maintenance_deadline.is_some_and(|value| now >= value);
                let explicit_deadline_due = deadline.is_some_and(|value| now >= value);
                if maintenance_due {
                    if !turn.session_source.is_non_root_agent() {
                        nudged_assignment_ids = nudge_stalled_assignments(
                            session.as_ref(),
                            turn.as_ref(),
                            store.as_ref(),
                            root_session_id.as_deref(),
                        )
                        .await;
                    }
                    maintenance_deadline = if maintenance_interval.is_zero() {
                        None
                    } else {
                        Some(now + maintenance_interval)
                    };
                    wake_read = read_wake_events(
                        store.as_ref(),
                        root_session_id.as_deref(),
                        cursor,
                    )
                    .await
                    .map_err(|error| {
                        turn.turn_timing_state
                            .record_internally_drained_waits(drained_event_pages);
                        FunctionCallError::RespondToModel(format!(
                            "wait_agent could not reread durable typed-task progress after maintenance: {error}"
                        ))
                    })?;
                    if wake_read.updated_agents.is_empty() {
                        unchanged_store_polls = unchanged_store_polls.saturating_add(1);
                    } else {
                        outcome = WaitOutcome::DurableActivity;
                    }
                    if outcome == WaitOutcome::BoundaryElapsed && !nudged_assignment_ids.is_empty()
                    {
                        outcome = WaitOutcome::MaintenanceActivity;
                    }
                }
                if outcome == WaitOutcome::BoundaryElapsed && explicit_deadline_due {
                    outcome = WaitOutcome::TimedOut;
                    wake_read.timed_out = true;
                }
                if outcome == WaitOutcome::BoundaryElapsed {
                    continue;
                }
            }

            if !wake_read.updated_agents.is_empty() {
                prove_durable_forward_progress(cursor, &wake_read).map_err(|message| {
                    turn.turn_timing_state
                        .record_internally_drained_waits(drained_event_pages);
                    FunctionCallError::RespondToModel(message)
                })?;
            }

            if wake_read.updated_agents.is_empty() {
                break (outcome, wake_read, HydratedAssignments::default());
            }
            let (Some(store), Some(root_session_id)) = (store.as_ref(), root_session_id.as_deref())
            else {
                break (outcome, wake_read, HydratedAssignments::default());
            };

            if cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "wait_agent cancelled".to_string(),
                ));
            }
            let first_page = wake_read;
            let first_assignment_ids = wake_assignment_ids(&first_page);
            let first_hydration = hydrate_assignments(coordinator, &first_assignment_ids)
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "wait_agent could not hydrate its first durable event page: {error}"
                    ))
                })?;
            let mut drain = WakeEventDrain::new(cursor, first_page.clone(), first_hydration)
                .map_err(FunctionCallError::RespondToModel)?;
            let mut drain_outcome = outcome;
            let mut fail_open = false;

            while drain.should_continue() {
                let Some(next_cursor) = drain.cursor() else {
                    fail_open = true;
                    break;
                };
                let next_page = match read_next_wake_page(
                    store,
                    root_session_id,
                    next_cursor,
                    &mut activity_rx,
                    &mut activity_open,
                    &cancellation_token,
                )
                .await
                {
                    BacklogPageRead::Page(Ok(page)) => page,
                    BacklogPageRead::Page(Err(error)) => {
                        tracing::debug!(
                            %error,
                            cursor = %next_cursor,
                            "wait_agent backlog drain failed open after a durable page read error"
                        );
                        fail_open = true;
                        break;
                    }
                    BacklogPageRead::Activity(activity) => {
                        drain_outcome = activity;
                        break;
                    }
                    BacklogPageRead::Cancelled => {
                        turn.turn_timing_state.record_internally_drained_waits(
                            u32::try_from(drain.internally_drained_pages()).unwrap_or(u32::MAX),
                        );
                        return Err(FunctionCallError::RespondToModel(
                            "wait_agent cancelled".to_string(),
                        ));
                    }
                };
                if cancellation_token.is_cancelled() {
                    turn.turn_timing_state.record_internally_drained_waits(
                        u32::try_from(drain.internally_drained_pages()).unwrap_or(u32::MAX),
                    );
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
                if let Some(activity) = take_pending_activity(&mut activity_rx, &mut activity_open)
                {
                    drain_outcome = activity;
                    break;
                }

                let mut assignment_ids = drain.assignment_ids().clone();
                assignment_ids.extend(wake_assignment_ids(&next_page));
                let hydration = match hydrate_assignments(coordinator, &assignment_ids).await {
                    Ok(hydration) => hydration,
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            cursor = %next_cursor,
                            "wait_agent backlog drain failed open after assignment hydration failed"
                        );
                        fail_open = true;
                        break;
                    }
                };
                if cancellation_token.is_cancelled() {
                    turn.turn_timing_state.record_internally_drained_waits(
                        u32::try_from(drain.internally_drained_pages()).unwrap_or(u32::MAX),
                    );
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
                if let Some(activity) = take_pending_activity(&mut activity_rx, &mut activity_open)
                {
                    drain_outcome = activity;
                    break;
                }
                match drain.push(next_page, hydration) {
                    WakePageAcceptance::Accepted => {}
                    WakePageAcceptance::AggregateBoundReached => break,
                    WakePageAcceptance::FailOpen(message) => {
                        tracing::debug!(
                            reason = message,
                            cursor = %next_cursor,
                            "wait_agent backlog drain could not prove a safe continuation"
                        );
                        fail_open = true;
                        break;
                    }
                }
            }

            if !fail_open && drain_outcome == WaitOutcome::DurableActivity {
                if cancellation_token.is_cancelled() {
                    turn.turn_timing_state.record_internally_drained_waits(
                        u32::try_from(drain.internally_drained_pages()).unwrap_or(u32::MAX),
                    );
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
                if let Some(activity) = take_pending_activity(&mut activity_rx, &mut activity_open)
                {
                    drain_outcome = activity;
                } else if drain.internally_drained_pages() > 0 {
                    match hydrate_assignments(coordinator, drain.assignment_ids()).await {
                        Ok(hydration) => {
                            if let Err(message) = drain.replace_if_same_revisions(hydration) {
                                tracing::debug!(
                                    reason = message,
                                    "wait_agent backlog drain failed open at its final task revision fence"
                                );
                                fail_open = true;
                            }
                        }
                        Err(error) => {
                            tracing::debug!(
                                %error,
                                "wait_agent backlog drain failed open at its final task hydration fence"
                            );
                            fail_open = true;
                        }
                    }
                }
            }

            if fail_open {
                let hydration = hydrate_assignments(coordinator, &first_assignment_ids)
                    .await
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "wait_agent could not fail open to its first durable event page: {error}"
                        ))
                    })?;
                drain = WakeEventDrain::new(cursor, first_page, hydration)
                    .map_err(FunctionCallError::RespondToModel)?;
            }

            let next_cursor = drain.cursor().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "wait_agent durable wake omitted its cursor revision".to_string(),
                )
            })?;
            if !explicit_cursor {
                let advanced = store
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
                    })?;
                if !advanced {
                    cursor = store
                        .automatic_wake_cursor(
                            root_session_id.to_string(),
                            consuming_agent_path.clone(),
                        )
                        .await
                        .map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "wait_agent could not reread its automatic cursor: {error}"
                            ))
                        })?;
                    continue 'wait_owner;
                }
            }
            drained_event_pages =
                u32::try_from(drain.internally_drained_pages()).unwrap_or(u32::MAX);
            let (wake_read, hydrated_assignments) = drain.finish();
            break (drain_outcome, wake_read, hydrated_assignments);
        };
        turn.turn_timing_state
            .record_internally_drained_waits(drained_event_pages);
        let mut typed_deltas = Vec::with_capacity(wake_read.updated_agents.len());
        let owner_assignments = hydrate_wait_owner_assignments(
            coordinator,
            store.as_ref(),
            root_session_id.as_deref(),
            &consuming_agent_path,
        )
        .await
        .map_err(FunctionCallError::RespondToModel)?;
        let owner_states = authoritative_wait_states(&owner_assignments).ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "wait_agent could not bind its authoritative wait to exact task revisions"
                    .to_string(),
            )
        })?;
        for event in &wake_read.updated_agents {
            let task = &hydrated_assignments
                .tasks
                .get(&event.assignment_id)
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(format!(
                        "wait_agent lost hydrated assignment {}",
                        event.assignment_id
                    ))
                })?;
            coordinator.record_first_meaningful_progress_once(
                event.attempt_id,
                event.reason,
                &task.assignment.created_at,
                &event.created_at,
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
        let mut result = WaitAgentResult::from_outcome(
            outcome,
            wake_read
                .latest_event_id
                .map(|event_id| event_id.to_string()),
            typed_deltas,
            wake_read.truncated_count,
            nudged_assignment_ids,
        );
        result.authoritative_wait_signal = authoritative_wait_signal(
            root_session_id.as_deref(),
            &consuming_agent_path,
            &owner_states,
            Some(&result.message),
        );
        if drained_event_pages > 0 {
            let resource_identity_hash = sha256_text(&format!(
                "agent-event-wait\0{}\0{}",
                root_session_id.as_deref().unwrap_or_default(),
                consuming_agent_path,
            ));
            let state_revision = sha256_text(
                &json!({
                    "cursor": &result.cursor,
                    "typed_deltas": &result.typed_deltas,
                    "truncated_count": result.truncated_count,
                    "nudged_assignment_ids": &result.nudged_assignment_ids,
                    "timed_out": result.timed_out,
                })
                .to_string(),
            );
            result.deterministic_continuation_receipts.push(
                TurnTimingDeterministicContinuationReceipt {
                    class: DeterministicContinuationClass::AgentEventWait,
                    wire_identity: String::new(),
                    resource_identity_hash,
                    state_revision,
                    host_action: DeterministicContinuationHostAction::AwaitStateChange,
                    action_bounds_hash: sha256_text(
                        &json!({
                            "cursor": &result.cursor,
                            "internally_drained_event_pages": drained_event_pages,
                            "max_event_pages": MAX_WAKE_EVENT_DRAIN_PAGES,
                            "max_events": MAX_WAKE_EVENTS_PER_ROOT,
                            "termination": "backlog-exhausted-or-revision-change-or-input-or-terminal-or-error-or-aggregate-bound-or-cancellation",
                        })
                        .to_string(),
                    ),
                    suppressed_continuation_count: drained_event_pages,
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
                    WaitOutcome::MaintenanceActivity => "maintenance_activity",
                    WaitOutcome::Steered => "steered",
                    WaitOutcome::BoundaryElapsed => "boundary_elapsed",
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

#[derive(Clone, Default)]
struct HydratedAssignments {
    revisions: BTreeMap<AssignmentId, String>,
    tasks: BTreeMap<AssignmentId, AgentTask>,
}

#[derive(Debug, Eq, PartialEq)]
enum WakePageAcceptance {
    Accepted,
    AggregateBoundReached,
    FailOpen(String),
}

struct WakeEventDrain {
    combined: WakeRead,
    hydrated_assignments: HydratedAssignments,
    assignment_ids: BTreeSet<AssignmentId>,
    seen_event_ids: HashSet<WakeEventId>,
    page_count: usize,
}

impl WakeEventDrain {
    fn new(
        cursor: Option<WakeEventId>,
        mut first: WakeRead,
        hydrated_assignments: HydratedAssignments,
    ) -> Result<Self, String> {
        prove_durable_forward_progress(cursor, &first)?;
        if first.updated_agents.len() > MAX_WAKE_EVENTS_PER_ROOT {
            return Err(
                "wait_agent first durable page exceeded the retained-event bound".to_string(),
            );
        }
        let assignment_ids = wake_assignment_ids(&first);
        if !assignment_ids
            .iter()
            .all(|assignment_id| hydrated_assignments.revisions.contains_key(assignment_id))
        {
            return Err("wait_agent first durable page omitted an assignment revision".to_string());
        }
        let seen_event_ids = first
            .updated_agents
            .iter()
            .map(|event| event.event_id)
            .collect();
        first.truncated_count = first
            .lost_to_retention_count
            .saturating_add(first.remaining_count);
        Ok(Self {
            combined: first,
            hydrated_assignments,
            assignment_ids,
            seen_event_ids,
            page_count: 1,
        })
    }

    fn push(
        &mut self,
        page: WakeRead,
        hydrated_assignments: HydratedAssignments,
    ) -> WakePageAcceptance {
        if self.page_count >= MAX_WAKE_EVENT_DRAIN_PAGES
            || self
                .combined
                .updated_agents
                .len()
                .saturating_add(page.updated_agents.len())
                > MAX_WAKE_EVENTS_PER_ROOT
        {
            return WakePageAcceptance::AggregateBoundReached;
        }
        let Some(cursor) = self.cursor() else {
            return WakePageAcceptance::FailOpen(
                "the accepted wake page omitted its exact cursor".to_string(),
            );
        };
        if let Err(message) = prove_durable_forward_progress(Some(cursor), &page) {
            return WakePageAcceptance::FailOpen(message);
        }
        if page.lost_to_retention_count != 0 {
            return WakePageAcceptance::FailOpen(
                "an exact retained wake cursor unexpectedly crossed the retention floor"
                    .to_string(),
            );
        }
        if page
            .updated_agents
            .iter()
            .any(|event| self.seen_event_ids.contains(&event.event_id))
        {
            return WakePageAcceptance::FailOpen(
                "durable wake pagination repeated an accepted event".to_string(),
            );
        }
        if let Err(message) = self.verify_same_revisions(&hydrated_assignments) {
            return WakePageAcceptance::FailOpen(message);
        }
        let page_assignment_ids = wake_assignment_ids(&page);
        if !page_assignment_ids
            .iter()
            .all(|assignment_id| hydrated_assignments.revisions.contains_key(assignment_id))
        {
            return WakePageAcceptance::FailOpen(
                "durable wake pagination omitted a task revision".to_string(),
            );
        }

        self.seen_event_ids
            .extend(page.updated_agents.iter().map(|event| event.event_id));
        self.assignment_ids.extend(page_assignment_ids);
        self.hydrated_assignments = hydrated_assignments;
        self.combined.reason = page.reason.or(self.combined.reason);
        self.combined.updated_agents.extend(page.updated_agents);
        self.combined.latest_event_id = page.latest_event_id;
        self.combined.remaining_count = page.remaining_count;
        self.combined.truncated_count = self
            .combined
            .lost_to_retention_count
            .saturating_add(self.combined.remaining_count);
        self.combined.timed_out = false;
        self.page_count = self.page_count.saturating_add(1);
        WakePageAcceptance::Accepted
    }

    fn should_continue(&self) -> bool {
        self.combined.remaining_count > 0
            && self.page_count < MAX_WAKE_EVENT_DRAIN_PAGES
            && self.combined.updated_agents.len() < MAX_WAKE_EVENTS_PER_ROOT
            && !self
                .hydrated_assignments
                .tasks
                .values()
                .any(|task| task.current_attempt.state.is_terminal())
    }

    fn cursor(&self) -> Option<WakeEventId> {
        self.combined.latest_event_id
    }

    fn assignment_ids(&self) -> &BTreeSet<AssignmentId> {
        &self.assignment_ids
    }

    fn verify_same_revisions(
        &self,
        hydrated_assignments: &HydratedAssignments,
    ) -> Result<(), String> {
        for (assignment_id, accepted) in &self.hydrated_assignments.revisions {
            let Some(current) = hydrated_assignments.revisions.get(assignment_id) else {
                return Err("durable wake pagination lost an assignment revision".to_string());
            };
            if current != accepted {
                return Err(format!(
                    "assignment {assignment_id} changed revision during durable wake pagination"
                ));
            }
        }
        Ok(())
    }

    fn replace_if_same_revisions(
        &mut self,
        hydrated_assignments: HydratedAssignments,
    ) -> Result<(), String> {
        self.verify_same_revisions(&hydrated_assignments)?;
        self.hydrated_assignments = hydrated_assignments;
        Ok(())
    }

    fn internally_drained_pages(&self) -> usize {
        self.page_count.saturating_sub(1)
    }

    fn finish(self) -> (WakeRead, HydratedAssignments) {
        (self.combined, self.hydrated_assignments)
    }
}

fn prove_durable_forward_progress(
    cursor: Option<WakeEventId>,
    wake_read: &WakeRead,
) -> Result<(), String> {
    if wake_read.updated_agents.is_empty() {
        return Err("wait_agent durable wake contained no advancing event".to_string());
    }
    let Some(latest) = wake_read.latest_event_id else {
        return Err("wait_agent durable wake omitted its cursor revision".to_string());
    };
    if cursor == Some(latest) {
        return Err("wait_agent durable wake repeated its cursor identity".to_string());
    }
    if wake_read.updated_agents.last().map(|event| event.event_id) != Some(latest) {
        return Err("wait_agent durable wake cursor did not identify its final event".to_string());
    }
    let mut page_ids = HashSet::with_capacity(wake_read.updated_agents.len());
    for event in &wake_read.updated_agents {
        if cursor == Some(event.event_id) || !page_ids.insert(event.event_id) {
            return Err("wait_agent durable wake page repeated an event identity".to_string());
        }
    }
    if wake_read.truncated_count
        != wake_read
            .lost_to_retention_count
            .saturating_add(wake_read.remaining_count)
    {
        return Err("wait_agent durable wake omitted exact retained-event bounds".to_string());
    }
    Ok(())
}

fn wake_assignment_ids(wake_read: &WakeRead) -> BTreeSet<AssignmentId> {
    wake_read
        .updated_agents
        .iter()
        .map(|event| event.assignment_id)
        .collect()
}

async fn hydrate_assignments(
    coordinator: &crate::agent::task_coordinator::AgentTaskCoordinator,
    assignment_ids: &BTreeSet<AssignmentId>,
) -> Result<HydratedAssignments, String> {
    let mut hydrated = HydratedAssignments::default();
    for assignment_id in assignment_ids {
        let task = coordinator
            .get_agent_task(*assignment_id, Some(0))
            .await
            .map_err(|error| format!("assignment {assignment_id}: {error}"))?;
        let revision = agent_task_revision(&task)
            .map_err(|error| format!("assignment {assignment_id} revision: {error}"))?;
        hydrated.revisions.insert(*assignment_id, revision);
        hydrated.tasks.insert(*assignment_id, task);
    }
    Ok(hydrated)
}

fn agent_task_revision(task: &AgentTask) -> Result<String, serde_json::Error> {
    serde_json::to_vec(task).map(|bytes| format!("{:x}", Sha256::digest(bytes)))
}

async fn hydrate_wait_owner_assignments(
    coordinator: &crate::agent::task_coordinator::AgentTaskCoordinator,
    store: Option<&std::sync::Arc<dyn codex_agent_task_store::AgentTaskStore>>,
    root_session_id: Option<&str>,
    consuming_agent_path: &str,
) -> Result<HydratedAssignments, String> {
    let (Some(store), Some(root_session_id)) = (store, root_session_id) else {
        return Ok(HydratedAssignments::default());
    };
    let descendant_prefix = format!("{consuming_agent_path}/");
    let assignment_ids = store
        .list_agent_task_bindings(root_session_id.to_string(), Some(256))
        .await
        .map_err(|error| format!("wait_agent could not list its durable task owners: {error}"))?
        .into_iter()
        .filter(|binding| binding.agent_path.starts_with(&descendant_prefix))
        .map(|binding| binding.assignment_id)
        .collect::<BTreeSet<_>>();
    hydrate_assignments(coordinator, &assignment_ids).await
}

fn wait_owner_is_settled(assignments: &HydratedAssignments) -> bool {
    !assignments.tasks.is_empty()
        && assignments.tasks.values().all(|task| {
            task.workspace_status.pending_gates.is_empty()
                && matches!(
                    task.current_attempt.state,
                    AttemptState::Completed
                        | AttemptState::Violated
                        | AttemptState::Abandoned
                        | AttemptState::NeedsMain
                )
        })
}

enum BacklogPageRead {
    Page(codex_agent_task_store::StoreResult<WakeRead>),
    Activity(WaitOutcome),
    Cancelled,
}

async fn read_next_wake_page(
    store: &std::sync::Arc<dyn codex_agent_task_store::AgentTaskStore>,
    root_session_id: &str,
    cursor: WakeEventId,
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    activity_open: &mut bool,
    cancellation_token: &CancellationToken,
) -> BacklogPageRead {
    loop {
        let page = store.read_wake_events(root_session_id.to_string(), Some(cursor));
        let activity_changed = async {
            if *activity_open {
                activity_rx.changed().await
            } else {
                std::future::pending().await
            }
        };
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => return BacklogPageRead::Cancelled,
            changed = activity_changed => {
                match changed {
                    Ok(()) => {
                        let outcome = match *activity_rx.borrow_and_update() {
                            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
                            InputQueueActivity::Steer => WaitOutcome::Steered,
                        };
                        return BacklogPageRead::Activity(outcome);
                    }
                    Err(_) => *activity_open = false,
                }
            }
            page = page => return BacklogPageRead::Page(page),
        }
    }
}

fn take_pending_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    activity_open: &mut bool,
) -> Option<WaitOutcome> {
    if !*activity_open {
        return None;
    }
    match activity_rx.has_changed() {
        Ok(true) => Some(match *activity_rx.borrow_and_update() {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        }),
        Ok(false) => None,
        Err(_) => {
            *activity_open = false;
            None
        }
    }
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

    fn test_authoritative_wait_state(
        assignment_id: &str,
        attempt_id: &str,
        state: AttemptState,
        epoch: u64,
        next_required_action: Option<&str>,
        receipt_available: bool,
        has_pending_gates: bool,
        task_revision: &str,
    ) -> AuthoritativeWaitState {
        AuthoritativeWaitState {
            assignment_id: assignment_id.to_string(),
            attempt_id: attempt_id.to_string(),
            state,
            epoch,
            next_required_action: next_required_action.map(str::to_string),
            receipt_available,
            has_pending_gates,
            task_revision: task_revision.to_string(),
        }
    }

    #[test]
    fn authoritative_signal_requires_unambiguous_typed_blocked_or_terminal_state() {
        let blocked = vec![test_authoritative_wait_state(
            "assignment-1",
            "attempt-1",
            AttemptState::NeedsMain,
            7,
            Some("main agent action"),
            true,
            false,
            "task-revision-1",
        )];
        assert_eq!(
            authoritative_wait_signal(Some("root"), "/root", &blocked, Some("blocked message"))
                .and_then(|signal| signal
                    .pointer("/authoritative_wait_owner_v1/disposition")
                    .cloned()),
            Some(json!("blocked"))
        );
        assert_eq!(
            authoritative_wait_signal(Some("root"), "/root", &blocked, Some("blocked message"))
                .and_then(|signal| signal
                    .pointer("/authoritative_wait_owner_v1/surfaceable_message")
                    .cloned()),
            None,
            "blocked owner output must never be promoted into terminal assistant text"
        );

        let terminal = vec![test_authoritative_wait_state(
            "assignment-1",
            "attempt-1",
            AttemptState::Completed,
            8,
            None,
            true,
            false,
            "task-revision-2",
        )];
        assert_eq!(
            authoritative_wait_signal(Some("root"), "/root", &terminal, Some("Wait completed."))
                .and_then(|signal| signal
                    .pointer("/authoritative_wait_owner_v1/disposition")
                    .cloned()),
            Some(json!("terminal"))
        );
        assert_eq!(
            authoritative_wait_signal(Some("root"), "/root", &terminal, Some("Wait completed."))
                .and_then(|signal| signal
                    .pointer("/authoritative_wait_owner_v1/surfaceable_message")
                    .cloned()),
            Some(json!("Wait completed."))
        );

        let active = vec![test_authoritative_wait_state(
            "assignment-1",
            "attempt-1",
            AttemptState::Active,
            9,
            None,
            false,
            false,
            "task-revision-3",
        )];
        assert!(
            authoritative_wait_signal(Some("root"), "/root", &active, Some("active")).is_none()
        );

        let pending_gate = vec![test_authoritative_wait_state(
            "assignment-1",
            "attempt-1",
            AttemptState::Completed,
            10,
            Some("review required"),
            true,
            true,
            "task-revision-4",
        )];
        assert!(
            authoritative_wait_signal(
                Some("root"),
                "/root",
                &pending_gate,
                Some("premature completion"),
            )
            .is_none(),
            "a completed attempt with pending gates is not terminal"
        );
    }

    #[test]
    fn authoritative_wait_snapshot_tracks_complete_task_revision() {
        let assignment_id = AssignmentId::new();
        let attempt_id = codex_agent_task_store::AttemptId::new();
        let now = chrono::Utc::now();
        let mut task = AgentTask {
            assignment: codex_agent_task_store::Assignment {
                assignment_id,
                root_session_id: "root".to_string(),
                admission_origin: codex_agent_task_store::AssignmentAdmissionOrigin::Typed,
                repository_id: "repository".to_string(),
                workspace_id: "workspace".to_string(),
                role: codex_agent_task_store::AgentRole::Worker,
                capability_profile: codex_agent_task_store::CapabilityProfile::ScopedSourceWrite,
                objective: "complete task".to_string(),
                acceptance_criteria: vec![codex_agent_task_store::AcceptanceCriterion {
                    id: "criterion".to_string(),
                    text: "criterion passes".to_string(),
                }],
                read_scope: Vec::new(),
                write_scope: Vec::new(),
                stop_condition: "done".to_string(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: Vec::new(),
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: codex_agent_task_store::WorkspaceStrategy::Shared,
                start_epoch: 7,
                relation: None,
                architecture_contract_ref: None,
                integration_plan: codex_agent_task_store::IntegrationPlan::SingleWriter,
                task_capsule: None,
                created_at: now,
            },
            current_attempt: codex_agent_task_store::Attempt {
                attempt_id,
                assignment_id,
                ordinal: 0,
                amendment: None,
                state: AttemptState::NeedsMain,
                created_at: now,
                sealed_at: Some(now),
            },
            gates: Vec::new(),
            receipt: None,
            validation_calls: Vec::new(),
            workspace_status: codex_agent_task_store::WorkspaceTaskStatus {
                epoch: 7,
                next_required_action: Some("main agent action".to_string()),
                ..Default::default()
            },
            isolation_handoff: None,
            integration_handoffs: Vec::new(),
            observations: Vec::new(),
        };
        let before_state =
            authoritative_wait_state(&task, agent_task_revision(&task).expect("task serializes"));
        let before = authoritative_wait_snapshot(Some("root"), "/root", &[before_state.clone()])
            .expect("blocked snapshot");

        task.gates.push(codex_agent_task_store::AgentGate {
            assignment_id,
            kind: codex_agent_task_store::GateKind::Review,
            status: codex_agent_task_store::GateStatus::Pending,
            reason: "review required".to_string(),
            waiver_reason: None,
            evidence_epoch: 7,
            updated_at: now,
            sealed_at: None,
        });
        let after_state = authoritative_wait_state(
            &task,
            agent_task_revision(&task).expect("changed task serializes"),
        );
        let after = authoritative_wait_snapshot(Some("root"), "/root", &[after_state.clone()])
            .expect("changed blocked snapshot");

        assert_eq!(after_state.state, before_state.state);
        assert_eq!(after_state.epoch, before_state.epoch);
        assert_eq!(
            after_state.next_required_action,
            before_state.next_required_action
        );
        assert_ne!(after.state_revision, before.state_revision);

        task.current_attempt.state = AttemptState::Completed;
        task.workspace_status.pending_gates = vec![codex_agent_task_store::GateKind::Review];
        let mut assignments = HydratedAssignments::default();
        assignments.tasks.insert(assignment_id, task.clone());
        assert!(
            !wait_owner_is_settled(&assignments),
            "pending gates keep a terminal attempt unsettled"
        );
        task.workspace_status.pending_gates.clear();
        assignments.tasks.insert(assignment_id, task);
        assert!(wait_owner_is_settled(&assignments));
    }

    #[test]
    fn durable_progress_rejects_missing_and_repeated_cursor_revisions() {
        let cursor = WakeEventId::new();
        let missing = WakeRead {
            reason: None,
            updated_agents: Vec::new(),
            latest_event_id: None,
            lost_to_retention_count: 0,
            remaining_count: 0,
            truncated_count: 0,
            timed_out: false,
        };
        assert!(prove_durable_forward_progress(Some(cursor), &missing).is_err());

        let repeated = WakeRead {
            latest_event_id: Some(cursor),
            ..missing
        };
        assert!(prove_durable_forward_progress(Some(cursor), &repeated).is_err());
    }

    fn test_wake_events(
        assignment_id: AssignmentId,
        attempt_id: codex_agent_task_store::AttemptId,
        count: usize,
    ) -> Vec<codex_agent_task_store::WakeEvent> {
        (0..count)
            .map(|index| codex_agent_task_store::WakeEvent {
                event_id: WakeEventId::new(),
                assignment_id,
                attempt_id,
                reason: codex_agent_task_store::ObservationKind::Reading,
                summary: format!("event {index}"),
                created_at: chrono::Utc::now(),
            })
            .collect()
    }

    fn test_wake_page(
        events: Vec<codex_agent_task_store::WakeEvent>,
        lost_to_retention_count: u64,
        remaining_count: u64,
    ) -> WakeRead {
        WakeRead {
            reason: events.last().map(|event| event.reason),
            latest_event_id: events.last().map(|event| event.event_id),
            updated_agents: events,
            lost_to_retention_count,
            remaining_count,
            truncated_count: lost_to_retention_count.saturating_add(remaining_count),
            timed_out: false,
        }
    }

    fn test_hydration(assignment_id: AssignmentId, revision: &str) -> HydratedAssignments {
        HydratedAssignments {
            revisions: BTreeMap::from([(assignment_id, revision.to_string())]),
            tasks: BTreeMap::new(),
        }
    }

    #[test]
    fn one_wake_page_is_returned_unchanged() {
        let assignment_id = AssignmentId::new();
        let attempt_id = codex_agent_task_store::AttemptId::new();
        let first = test_wake_page(test_wake_events(assignment_id, attempt_id, 3), 0, 0);
        let drain = WakeEventDrain::new(None, first.clone(), test_hydration(assignment_id, "r1"))
            .expect("first page is valid");

        assert_eq!(drain.internally_drained_pages(), 0);
        assert_eq!(drain.finish().0, first);
    }

    #[test]
    fn two_through_six_wake_pages_drain_once_in_store_order() {
        for page_count in 2..=MAX_WAKE_EVENT_DRAIN_PAGES {
            let assignment_id = AssignmentId::new();
            let attempt_id = codex_agent_task_store::AttemptId::new();
            let pages = (0..page_count)
                .map(|page_index| {
                    let remaining = (page_count - page_index - 1) * 2;
                    test_wake_page(
                        test_wake_events(assignment_id, attempt_id, 2),
                        u64::from(page_index == 0 && page_count == MAX_WAKE_EVENT_DRAIN_PAGES),
                        remaining as u64,
                    )
                })
                .collect::<Vec<_>>();
            let expected_ids = pages
                .iter()
                .flat_map(|page| page.updated_agents.iter().map(|event| event.event_id))
                .collect::<Vec<_>>();
            let mut drain = WakeEventDrain::new(
                None,
                pages[0].clone(),
                test_hydration(assignment_id, "stable"),
            )
            .expect("first page is valid");
            for page in pages.iter().skip(1).cloned() {
                assert_eq!(
                    drain.push(page, test_hydration(assignment_id, "stable")),
                    WakePageAcceptance::Accepted
                );
            }

            assert_eq!(drain.internally_drained_pages(), page_count - 1);
            let (combined, _) = drain.finish();
            let actual_ids = combined
                .updated_agents
                .iter()
                .map(|event| event.event_id)
                .collect::<Vec<_>>();
            assert_eq!(actual_ids, expected_ids);
            assert_eq!(
                actual_ids.iter().copied().collect::<HashSet<_>>().len(),
                actual_ids.len()
            );
            assert_eq!(combined.remaining_count, 0);
        }
    }

    #[test]
    fn changed_task_or_event_revision_fails_open_to_first_page() {
        let assignment_id = AssignmentId::new();
        let attempt_id = codex_agent_task_store::AttemptId::new();
        let first = test_wake_page(test_wake_events(assignment_id, attempt_id, 1), 0, 1);
        let second = test_wake_page(test_wake_events(assignment_id, attempt_id, 1), 0, 0);
        let mut changed_task = WakeEventDrain::new(
            None,
            first.clone(),
            test_hydration(assignment_id, "revision-1"),
        )
        .expect("first page is valid");
        assert!(matches!(
            changed_task.push(second.clone(), test_hydration(assignment_id, "revision-2")),
            WakePageAcceptance::FailOpen(_)
        ));
        assert_eq!(changed_task.finish().0, first);

        let mut changed_event = WakeEventDrain::new(
            None,
            first.clone(),
            test_hydration(assignment_id, "revision-1"),
        )
        .expect("first page is valid");
        let mut invalid_page = second;
        invalid_page.latest_event_id = Some(WakeEventId::new());
        assert!(matches!(
            changed_event.push(invalid_page, test_hydration(assignment_id, "revision-1")),
            WakePageAcceptance::FailOpen(_)
        ));
        assert_eq!(changed_event.finish().0, first);
    }

    #[test]
    fn wake_order_uses_store_order_not_uuid_numeric_order() {
        let assignment_id = AssignmentId::new();
        let attempt_id = codex_agent_task_store::AttemptId::new();
        let mut events = test_wake_events(assignment_id, attempt_id, 2);
        events.sort_by_key(|event| std::cmp::Reverse(event.event_id));
        let page = test_wake_page(events.clone(), 0, 0);

        prove_durable_forward_progress(None, &page).expect("store order is authoritative");
        assert_eq!(page.updated_agents, events);
    }

    #[test]
    fn wake_event_aggregate_bound_stops_without_partial_page_acceptance() {
        let assignment_id = AssignmentId::new();
        let attempt_id = codex_agent_task_store::AttemptId::new();
        let first = test_wake_page(
            test_wake_events(assignment_id, attempt_id, MAX_WAKE_EVENTS_PER_ROOT - 1),
            0,
            2,
        );
        let overflow = test_wake_page(test_wake_events(assignment_id, attempt_id, 2), 0, 0);
        let mut drain =
            WakeEventDrain::new(None, first.clone(), test_hydration(assignment_id, "stable"))
                .expect("first aggregate page is valid");

        assert_eq!(
            drain.push(overflow, test_hydration(assignment_id, "stable")),
            WakePageAcceptance::AggregateBoundReached
        );
        let (combined, _) = drain.finish();
        assert_eq!(combined, first);
        assert_eq!(combined.updated_agents.len(), MAX_WAKE_EVENTS_PER_ROOT - 1);
    }

    #[tokio::test]
    async fn maintenance_boundary_is_reported_to_the_wait_owner() {
        let (activity_tx, mut activity_rx) =
            tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let mut activity_open = true;
        let (outcome, _) = wait_for_activity(
            &mut activity_rx,
            &mut activity_open,
            None,
            Some(Instant::now() + Duration::from_millis(5)),
            None,
            None,
            None,
        )
        .await
        .expect("wait result");

        assert_eq!(outcome, WaitOutcome::BoundaryElapsed);
        drop(activity_tx);
    }

    #[tokio::test]
    async fn input_activity_wakes_before_owner_boundary() {
        let (_activity_tx, mut activity_rx) =
            tokio::sync::watch::channel(InputQueueActivity::Steer);
        let mut activity_open = true;
        let (outcome, wake_read) = wait_for_activity(
            &mut activity_rx,
            &mut activity_open,
            Some(InputQueueActivity::Steer),
            Some(Instant::now() + Duration::from_secs(1)),
            None,
            None,
            None,
        )
        .await
        .expect("wait result");

        assert_eq!(outcome, WaitOutcome::Steered);
        assert!(!wake_read.timed_out);
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
    #[serde(skip)]
    pub(crate) authoritative_wait_signal: Option<JsonValue>,
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
            WaitOutcome::MaintenanceActivity => "Wait maintenance produced a state change.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::BoundaryElapsed => "Wait maintenance boundary elapsed.",
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
            authoritative_wait_signal: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthoritativeWaitState {
    assignment_id: String,
    attempt_id: String,
    state: AttemptState,
    epoch: u64,
    next_required_action: Option<String>,
    receipt_available: bool,
    has_pending_gates: bool,
    task_revision: String,
}

fn authoritative_wait_states(
    assignments: &HydratedAssignments,
) -> Option<Vec<AuthoritativeWaitState>> {
    assignments
        .tasks
        .iter()
        .map(|(assignment_id, task)| {
            Some(authoritative_wait_state(
                task,
                assignments.revisions.get(assignment_id)?.clone(),
            ))
        })
        .collect()
}

fn authoritative_wait_state(task: &AgentTask, task_revision: String) -> AuthoritativeWaitState {
    AuthoritativeWaitState {
        assignment_id: task.assignment.assignment_id.to_string(),
        attempt_id: task.current_attempt.attempt_id.to_string(),
        state: task.current_attempt.state,
        epoch: task.workspace_status.epoch,
        next_required_action: task.workspace_status.next_required_action.clone(),
        receipt_available: task.receipt.is_some(),
        has_pending_gates: !task.workspace_status.pending_gates.is_empty(),
        task_revision,
    }
}

fn authoritative_wait_signal(
    root_session_id: Option<&str>,
    consuming_agent_path: &str,
    states: &[AuthoritativeWaitState],
    surfaceable_message: Option<&str>,
) -> Option<JsonValue> {
    if states.is_empty() || states.iter().any(|state| state.has_pending_gates) {
        return None;
    }
    let disposition = if states
        .iter()
        .any(|state| state.state == AttemptState::NeedsMain)
        && states.iter().all(|state| {
            matches!(
                state.state,
                AttemptState::Completed
                    | AttemptState::Violated
                    | AttemptState::Abandoned
                    | AttemptState::NeedsMain
            )
        }) {
        "blocked"
    } else if states.iter().all(|state| {
        matches!(
            state.state,
            AttemptState::Completed | AttemptState::Violated | AttemptState::Abandoned
        )
    }) {
        "terminal"
    } else {
        return None;
    };
    let mut states = states.to_vec();
    states.sort_by(|left, right| {
        left.assignment_id
            .cmp(&right.assignment_id)
            .then_with(|| left.attempt_id.cmp(&right.attempt_id))
    });
    let snapshot = authoritative_wait_snapshot(root_session_id, consuming_agent_path, &states)?;
    let receipt_identity = sha256_text(
        &serde_json::to_string(
            &states
                .iter()
                .map(|state| {
                    json!({
                        "assignment_id": state.assignment_id,
                        "attempt_id": state.attempt_id,
                        "epoch": state.epoch,
                        "receipt_available": state.receipt_available,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .ok()?,
    );
    let mut proof = json!({
        "adapter": "multi_agent_v2",
        "disposition": disposition,
        "owner": snapshot.owner,
        "state_revision": snapshot.state_revision,
        "receipt_identity": receipt_identity,
    });
    if disposition == "terminal"
        && let Some(message) = surfaceable_message.filter(|message| !message.trim().is_empty())
    {
        proof["surfaceable_message"] = JsonValue::String(message.to_string());
    }
    Some(json!({ "authoritative_wait_owner_v1": proof }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthoritativeWaitSnapshot {
    pub(crate) owner: String,
    pub(crate) state_revision: String,
}

fn authoritative_wait_snapshot(
    root_session_id: Option<&str>,
    consuming_agent_path: &str,
    states: &[AuthoritativeWaitState],
) -> Option<AuthoritativeWaitSnapshot> {
    if states.is_empty() {
        return None;
    }
    let mut states = states.to_vec();
    states.sort_by(|left, right| {
        left.assignment_id
            .cmp(&right.assignment_id)
            .then_with(|| left.attempt_id.cmp(&right.attempt_id))
    });
    let owner_revision_state = states
        .iter()
        .map(|state| {
            json!({
                "assignment_id": state.assignment_id,
                "task_revision": state.task_revision,
            })
        })
        .collect::<Vec<_>>();
    Some(AuthoritativeWaitSnapshot {
        owner: sha256_text(&format!(
            "multi-agent-v2-wait\0{}\0{}",
            root_session_id.unwrap_or_default(),
            consuming_agent_path,
        )),
        state_revision: sha256_text(&serde_json::to_string(&owner_revision_state).ok()?),
    })
}

pub(crate) async fn inspect_authoritative_wait_snapshot(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    assignment_ids: &[String],
) -> Option<AuthoritativeWaitSnapshot> {
    if assignment_ids.is_empty() {
        return None;
    }
    let coordinator = session.services.agent_control.task_coordinator();
    let root_session_id = coordinator.root_session_id()?;
    let consuming_agent_path = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root)
        .to_string();
    let guarded_assignment_ids = assignment_ids
        .iter()
        .map(|assignment_id| AssignmentId::parse(assignment_id).ok())
        .collect::<Option<BTreeSet<_>>>()?;
    let store = coordinator.store()?;
    let mut assignments = hydrate_wait_owner_assignments(
        coordinator,
        Some(&store),
        Some(&root_session_id),
        &consuming_agent_path,
    )
    .await
    .ok()?;
    assignments
        .tasks
        .retain(|assignment_id, _| guarded_assignment_ids.contains(assignment_id));
    assignments
        .revisions
        .retain(|assignment_id, _| guarded_assignment_ids.contains(assignment_id));
    if assignments.tasks.len() != guarded_assignment_ids.len() {
        return None;
    }
    let states = authoritative_wait_states(&assignments)?;
    authoritative_wait_snapshot(Some(&root_session_id), &consuming_agent_path, &states)
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

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        self.authoritative_wait_signal.clone()
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
    MaintenanceActivity,
    Steered,
    BoundaryElapsed,
    TimedOut,
}

fn earliest_deadline(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(std::cmp::min(first, second)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

async fn read_wake_events(
    store: Option<&std::sync::Arc<dyn codex_agent_task_store::AgentTaskStore>>,
    root_session_id: Option<&str>,
    cursor: Option<WakeEventId>,
) -> codex_agent_task_store::StoreResult<WakeRead> {
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
            lost_to_retention_count: 0,
            remaining_count: 0,
            truncated_count: 0,
            timed_out: false,
        }),
    }
}

async fn wait_wake_events(
    store: Option<&std::sync::Arc<dyn codex_agent_task_store::AgentTaskStore>>,
    root_session_id: Option<&str>,
    cursor: Option<WakeEventId>,
) -> codex_agent_task_store::StoreResult<WakeRead> {
    match (store, root_session_id) {
        (Some(store), Some(root_session_id)) => {
            store
                .wait_for_wake_events(root_session_id.to_string(), cursor)
                .await
        }
        _ => std::future::pending().await,
    }
}

// The explicit activity, deadline, durable-store, and cursor inputs document
// every wake source participating in this private wait state machine.
#[allow(clippy::too_many_arguments)]
async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    activity_open: &mut bool,
    pending_activity: Option<InputQueueActivity>,
    boundary_deadline: Option<Instant>,
    store: Option<&std::sync::Arc<dyn codex_agent_task_store::AgentTaskStore>>,
    root_session_id: Option<&str>,
    cursor: Option<WakeEventId>,
) -> codex_agent_task_store::StoreResult<(WaitOutcome, WakeRead)> {
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
                lost_to_retention_count: 0,
                remaining_count: 0,
                truncated_count: 0,
                timed_out: false,
            },
        ));
    }
    if boundary_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        let current = read_wake_events(store, root_session_id, cursor).await?;
        return Ok(if current.updated_agents.is_empty() {
            (WaitOutcome::BoundaryElapsed, current)
        } else {
            (WaitOutcome::DurableActivity, current)
        });
    }
    let durable_activity = wait_wake_events(store, root_session_id, cursor);
    let input_activity = async {
        if !*activity_open {
            return std::future::pending::<InputQueueActivity>().await;
        }
        match activity_rx.changed().await {
            Ok(()) => *activity_rx.borrow_and_update(),
            Err(_) => {
                *activity_open = false;
                std::future::pending::<InputQueueActivity>().await
            }
        }
    };
    let boundary = async {
        match boundary_deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        activity = input_activity => {
            let outcome = match activity {
                InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
                InputQueueActivity::Steer => WaitOutcome::Steered,
            };
            let wake_read = read_wake_events(store, root_session_id, cursor).await?;
            Ok((outcome, wake_read))
        }
        durable = durable_activity => Ok((WaitOutcome::DurableActivity, durable?)),
        _ = boundary => {
            let current = read_wake_events(store, root_session_id, cursor).await?;
            Ok(if current.updated_agents.is_empty() {
                (WaitOutcome::BoundaryElapsed, current)
            } else {
                (WaitOutcome::DurableActivity, current)
            })
        }
    }
}
