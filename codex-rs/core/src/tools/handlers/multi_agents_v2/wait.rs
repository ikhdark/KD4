use super::*;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AssignmentAdmissionOrigin;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::MAX_WAKE_EVENTS_PER_READ;
use codex_agent_task_store::MAX_WAKE_EVENTS_PER_ROOT;
use codex_agent_task_store::NonproductiveRecovery;
use codex_agent_task_store::WakeEventId;
use codex_agent_task_store::WakeRead;
use codex_agent_task_store::WakeReadStatus;
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
use std::sync::Arc;
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
        Self {
            options,
        }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // A passive wait must not exclude independent spawns or receipt reads
        // that can produce its wake. Cursor publication is already compare-and-
        // swap guarded by the store; cancellation remains owned by this handler.
        true
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
            step_context,
            payload,
            call_id,
            cancellation_token,
            source,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        let explicit_timeout_ms = match args.timeout_ms {
            Some(ms) if ms < min_timeout_ms => None,
            Some(ms) => Some(ms.min(max_timeout_ms)),
            None => None,
        };
        let maintenance_interval =
            Duration::from_millis(u64::try_from(default_timeout_ms.max(0)).unwrap_or(u64::MAX));

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (activity_rx, _) = session
            .input_queue
            .subscribe_agent_activity(turn_state.as_deref())
            .await;

        let mut input = AgentWaitInput {
            session: &session,
            turn_state: turn_state.as_deref(),
            receiver: activity_rx,
        };
        let wait_started = Instant::now();

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

        // Nudges sent while an explicit timeout keeps the wait open are reported by
        // whichever result ends it, including an input-only exit.
        let mut nudged_assignment_ids: Vec<String> = Vec::new();
        // Close the published item on ordinary validation, store and cancellation errors.
        let mut result: Result<WaitAgentResult, FunctionCallError> = async {
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
            macro_rules! before_delivery {
                ($future:expr, $cursor:expr) => {
                    tokio::select! {
                        biased;
                        _ = cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("wait_agent cancelled".to_string())),
                        outcome = input.next() => return Ok(input_only_result(&turn, outcome, $cursor, wait_started)),
                        result = $future => result,
                    }
                };
            }
            if cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel("wait_agent cancelled".to_string()));
            }
            if let Some(outcome) = input.current().await {
                return Ok(input_only_result(&turn, outcome, parsed_cursor, wait_started));
            }
            let coordinator = session.services.agent_control.task_coordinator();
            // Already-pending input does not need durable task-store state. Avoid
            // delaying immediate mailbox or steering delivery on store startup.
            if coordinator.store().is_none() {
                before_delivery!(coordinator
                    .initialize_for_workspace_coordination(
                        session.services.state_db.clone(),
                        turn.config.sqlite_home.clone(),
                        turn.config.model_provider_id.clone(),
                        session.services.agent_control.session_id().to_string(),
                    ), parsed_cursor)
                    .map_err(|error| {
                        task_store_context_error(
                            "wait_agent",
                            "wait_agent could not initialize durable typed-task progress",
                            error,
                        )
                    })?;
            }
            let store = coordinator.store();
            let root_session_id = coordinator.root_session_id();
            let consuming_agent_path = turn
                .session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root)
                .to_string();
            let cursor = match (explicit_cursor, store.as_ref(), root_session_id.as_deref()) {
                (false, Some(store), Some(root_session_id)) => before_delivery!(store
                    .automatic_wake_cursor(root_session_id.to_string(), consuming_agent_path.clone()), parsed_cursor)
                    .map_err(|error| {
                        task_store_context_error(
                            "wait_agent",
                            "wait_agent could not initialize its automatic cursor",
                            error,
                        )
                    })?,
                _ => parsed_cursor,
            };
            // Admission is not progress. Admission-only pages advance this scan
            // cursor without ending the wait; `cursor` stays the input-only report
            // and the compare-and-swap expectation until a result is delivered.
            let mut scan_cursor = cursor;
            let mut folded_admission_events = 0_usize;
            let deadline = earliest_deadline(
                explicit_timeout_ms
                    .map(|timeout_ms| wait_started + Duration::from_millis(timeout_ms as u64)),
                crate::unified_exec::nested_poll_bound(source.nested_deadline()),
            );
            let mut maintenance_deadline = Some(wait_started + maintenance_interval);
            let mut unchanged_store_polls = 0_u32;
            let mut drained_event_pages = 0_u32;
            let (outcome, wake_read, hydrated_assignments, settled_revision) = 'wait_owner: loop {
                if let Some(outcome) = input.current().await {
                    return Ok(input_only_result(&turn, outcome, cursor, wait_started));
                }
                {
                    let current = before_delivery!(read_wake_events(store.as_ref(), root_session_id.as_deref(), scan_cursor), cursor)
                        .map_err(|error| {
                            task_store_context_error(
                                "wait_agent",
                                "wait_agent could not inspect durable progress before waiting",
                                error,
                            )
                        })?;
                    if current.updated_agents.is_empty() {
                        let revision = match (store.as_ref(), root_session_id.as_deref()) {
                            (Some(store), Some(root)) => before_delivery!(store.task_state_revision_for_root(root), cursor).ok(),
                            _ => None,
                        };
                        let owners = before_delivery!(hydrate_wait_owner_assignments(
                            coordinator,
                            store.as_ref(),
                            root_session_id.as_deref(),
                            &consuming_agent_path,
                        ), cursor)
                        .map_err(FunctionCallError::RespondToModel)?;
                        if !owners.failures.is_empty() || !coordinator.unavailable_descendants().is_empty() || wait_owner_is_settled(&owners) {
                            break 'wait_owner (WaitOutcome::MaintenanceActivity, current, owners, revision);
                        }
                    }
                }
                let boundary_deadline = earliest_deadline(deadline, maintenance_deadline);
                let wait = wait_for_activity(
                    &mut input,
                    boundary_deadline,
                    store.as_ref(),
                    root_session_id.as_deref(),
                    scan_cursor,
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
                    task_store_context_error(
                        "wait_agent",
                        "wait_agent could not read durable typed-task progress",
                        error,
                    )
                })?;

                if outcome == WaitOutcome::BoundaryElapsed {
                    let now = Instant::now();
                    let maintenance_due = maintenance_deadline.is_some_and(|value| now >= value);
                    let explicit_deadline_due = deadline.is_some_and(|value| now >= value);
                    if maintenance_due && !explicit_deadline_due {
                        let maintenance = async {
                            let nudged = if !turn.session_source.is_non_root_agent() {
                                nudge_stalled_assignments(
                                    session.as_ref(),
                                    turn.as_ref(),
                                    store.as_ref(),
                                    root_session_id.as_deref(),
                                    turn_state.as_deref(),
                                )
                                .await
                            } else {
                                Vec::new()
                            };
                            let read = read_wake_events(
                                store.as_ref(),
                                root_session_id.as_deref(),
                                scan_cursor,
                            )
                            .await;
                            (nudged, read)
                        };
                        let completed = tokio::select! {
                            biased;
                            _ = cancellation_token.cancelled() => {
                                turn.turn_timing_state.record_internally_drained_waits(drained_event_pages);
                                return Err(FunctionCallError::RespondToModel("wait_agent cancelled".to_string()));
                            }
                            _ = async {
                                match deadline {
                                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                                    None => std::future::pending::<()>().await,
                                }
                            } => None,
                            completed = maintenance => Some(completed),
                        };
                        if let Some((nudged, read)) = completed {
                            let fresh_nudge = !nudged.is_empty();
                            for assignment_id in nudged {
                                if !nudged_assignment_ids.contains(&assignment_id) {
                                    nudged_assignment_ids.push(assignment_id);
                                }
                            }
                            maintenance_deadline = if maintenance_interval.is_zero() {
                                None
                            } else {
                                Some(now + maintenance_interval)
                            };
                            wake_read = read.map_err(|error| {
                                turn.turn_timing_state.record_internally_drained_waits(drained_event_pages);
                                task_store_context_error(
                                    "wait_agent",
                                    "wait_agent could not reread durable typed-task progress after maintenance",
                                    error,
                                )
                            })?;
                            if wake_read.updated_agents.is_empty() {
                                unchanged_store_polls = unchanged_store_polls.saturating_add(1);
                            } else {
                                outcome = WaitOutcome::DurableActivity;
                            }
                            // A nudge is maintenance, not a deliverable update. A caller
                            // that gave timeout_ms keeps waiting until its deadline and
                            // receives the accumulated nudges with that result.
                            if outcome == WaitOutcome::BoundaryElapsed
                                && fresh_nudge
                                && explicit_timeout_ms.is_none()
                            {
                                outcome = WaitOutcome::MaintenanceActivity;
                            }
                        } else {
                            outcome = WaitOutcome::TimedOut;
                            wake_read.timed_out = true;
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

                if let Some(outcome) = input.current().await {
                    return Ok(input_only_result(&turn, outcome, cursor, wait_started));
                }
                if matches!(outcome, WaitOutcome::Steered | WaitOutcome::MailboxActivity | WaitOutcome::DeferredCompletion) {
                    return Ok(input_only_result(&turn, outcome, cursor, wait_started));
                }
                if !wake_read.updated_agents.is_empty() {
                    prove_durable_forward_progress(scan_cursor, &wake_read).map_err(|message| {
                        turn.turn_timing_state
                            .record_internally_drained_waits(drained_event_pages);
                        FunctionCallError::RespondToModel(message)
                    })?;
                }

                if wake_read.updated_agents.is_empty() {
                    break (outcome, wake_read, HydratedAssignments::default(), None);
                }
                let (Some(store), Some(root_session_id)) = (store.as_ref(), root_session_id.as_deref())
                else {
                    break (outcome, wake_read, HydratedAssignments::default(), None);
                };

                if cancellation_token.is_cancelled() {
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
                let first_page = wake_read;
                let first_assignment_ids = wake_assignment_ids(&first_page);
                let first_hydration = before_delivery!(hydrate_assignments(coordinator, &first_assignment_ids), cursor)
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "wait_agent could not hydrate its first durable event page: {error}"
                        ))
                    })?;
                let mut drain = WakeEventDrain::new(scan_cursor, first_page.clone(), first_hydration)
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
                        &mut input,
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
                    if let Some(activity) = input.current().await
                    {
                        drain_outcome = activity;
                        break;
                    }

                    let assignment_ids = wake_assignment_ids(&next_page);
                    let hydration = match before_delivery!(hydrate_assignments(coordinator, &assignment_ids), cursor) {
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
                    if let Some(activity) = input.current().await
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
                    if let Some(activity) = input.current().await
                    {
                        drain_outcome = activity;
                    } else if drain.internally_drained_pages() > 0 {
                        let assignment_ids = drain.assignment_ids();
                        match before_delivery!(hydrate_assignments(coordinator, &assignment_ids), cursor) {
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
                    let hydration = before_delivery!(hydrate_assignments(coordinator, &first_assignment_ids), cursor)
                        .map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "wait_agent could not fail open to its first durable event page: {error}"
                            ))
                        })?;
                    drain = WakeEventDrain::new(scan_cursor, first_page, hydration)
                        .map_err(FunctionCallError::RespondToModel)?;
                }

                drain.cursor().ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "wait_agent durable wake omitted its cursor revision".to_string(),
                    )
                })?;
                drained_event_pages = drained_event_pages.saturating_add(
                    u32::try_from(drain.internally_drained_pages()).unwrap_or(u32::MAX),
                );
                let (wake_read, hydrated_assignments) = drain.finish();
                if drain_outcome == WaitOutcome::DurableActivity
                    && hydrated_assignments.failures.is_empty()
                    && is_admission_only(&wake_read)
                {
                    // Acknowledge admission in the scan and keep waiting; the
                    // count is reported with whichever result ends this wait.
                    folded_admission_events =
                        folded_admission_events.saturating_add(wake_read.updated_agents.len());
                    scan_cursor = wake_read.latest_event_id;
                    continue;
                }
                break (drain_outcome, wake_read, hydrated_assignments, None);
            };
            turn.turn_timing_state
                .record_internally_drained_waits(drained_event_pages);
            let mut typed_deltas = Vec::with_capacity(wake_read.updated_agents.len());
            if let Some(outcome) = input.current().await {
                return Ok(input_only_result(&turn, outcome, cursor, wait_started));
            }
            // Reuse the settled payload only behind a cross-connection fence.
            // Active/expiring leases are time-dependent and are never cached.
            let reusable = hydrated_assignments.tasks.values().all(|task| {
                !matches!(task.workspace_status.lease_state,
                    Some(codex_agent_task_store::LeaseState::Active | codex_agent_task_store::LeaseState::Expired))
            }) && match (settled_revision, store.as_ref(), root_session_id.as_deref()) {
                (Some(before), Some(store), Some(root)) => before_delivery!(store.task_state_revision_for_root(root), cursor).ok() == Some(before),
                _ => false,
            };
            let refreshed_owner_assignments;
            let owner_assignments = if reusable {
                &hydrated_assignments
            } else {
                refreshed_owner_assignments = before_delivery!(hydrate_wait_owner_assignments(
                    coordinator, store.as_ref(), root_session_id.as_deref(), &consuming_agent_path,
                ), cursor).map_err(FunctionCallError::RespondToModel)?;
                &refreshed_owner_assignments
            };
            let owner_states = authoritative_wait_states(owner_assignments).ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "wait_agent could not bind its authoritative wait to exact task revisions"
                        .to_string(),
                )
            })?;
            for event in &wake_read.updated_agents {
                let Some(task) = hydrated_assignments.tasks.get(&event.assignment_id) else {
                    if hydrated_assignments.failures.contains_key(&event.assignment_id) {
                        continue;
                    }
                    return Err(
                        FunctionCallError::RespondToModel(format!(
                            "wait_agent lost hydrated assignment {}",
                            event.assignment_id
                        ))
                    );
                };
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
            let mut failures = hydrated_assignments.failures.clone();
            failures.extend(owner_assignments.failures.clone());
            for (assignment_id, error) in failures {
                typed_deltas.push(json!({"assignment_id": assignment_id, "health": "unavailable",
                    "obligation_resolved": false, "receipt": durable_receipt_pointer(assignment_id.to_string(), false),
                    "error": error}));
            }
            let unresolved_descendants = coordinator.unavailable_descendants();
            for (thread_id, error) in &unresolved_descendants {
                typed_deltas.push(json!({"thread_id": thread_id, "health": "unavailable",
                    "obligation_resolved": false, "error": error}));
            }
            let mut result = WaitAgentResult::from_outcome(
                outcome,
                wake_read
                    .latest_event_id
                    .map(|event_id| event_id.to_string()),
                typed_deltas,
                wake_read.truncated_count,
                std::mem::take(&mut nudged_assignment_ids),
            );
            if outcome == WaitOutcome::MaintenanceActivity
                && result.nudged_assignment_ids.is_empty()
                && result.typed_deltas.is_empty()
                && wait_owner_is_settled(owner_assignments)
            {
                result.message = SETTLED_OWNER_MESSAGE.to_string();
            }
            if folded_admission_events > 0 {
                result
                    .message
                    .push_str(&admission_fold_note(folded_admission_events));
            }
            result.authoritative_wait_signal = unresolved_descendants.is_empty().then(|| authoritative_wait_signal(
                root_session_id.as_deref(),
                &consuming_agent_path,
                &owner_states,
                Some(&result.message),
            )).flatten();
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
                        WaitOutcome::DeferredCompletion => "deferred_completion",
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

            // Finish every fallible hydration before acknowledgment. Retain the
            // exact prepared result before atomically publishing its cursor and
            // recovery receipt. A lost/interrupted response remains addressable
            // from the next ordinary wait, including after a restart.
            if !explicit_cursor
                && let (Some(store), Some(root)) = (store.as_ref(), root_session_id.as_deref())
            {
                result.previous_delivery = before_delivery!(store.automatic_wake_delivery(root.to_string(), consuming_agent_path.clone()), cursor).map_err(|error| task_store_context_error("wait_agent", "wait_agent could not recover its prior delivery", error))?
                    .map(|value| serde_json::from_str(&value)).transpose()
                    .map_err(|error| FunctionCallError::RespondToModel(format!("wait_agent prior delivery is invalid: {error}")))?;
                if let Some(next) = result.cursor.as_deref().map(WakeEventId::parse).transpose()
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                    && Some(next) != cursor
                {
                    // Keep the preceding artifact pointer in this retained result.
                    // It is a bounded receipt, not an embedded result, and preserves
                    // recovery across multiple interrupted responses.
                    let retained = serde_json::to_value(&result).map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
                        &turn.config.codex_home, &session.thread_id.to_string(),
                        &codex_tools::CanonicalToolResult::json(retained),
                    ).await;
                    if !artifact.complete {
                        return Err(FunctionCallError::RespondToModel("wait_agent could not retain its delivery; cursor unchanged".to_string()));
                    }
                    let artifact_id = artifact.artifact_id().ok_or_else(|| FunctionCallError::RespondToModel("wait_agent retained delivery has no identity".to_string()))?;
                    let receipt = json!({"cursor": result.cursor, "artifact_id": artifact_id, "thread_id": session.thread_id, "recovery_tool": "read_tool_output"});
                    let published = store.publish_automatic_wake_delivery(root.to_string(), consuming_agent_path.clone(), cursor, next, receipt.to_string())
                        .await.map_err(|error| task_store_context_error("wait_agent", "wait_agent could not publish its retained delivery", error))?;
                    if !published {
                        return Err(FunctionCallError::RespondToModel("wait_agent concurrent consumer advanced the cursor; call wait_agent again to recover its retained delivery".to_string()));
                    }
                }
            }
            Ok(result)
        }
        .await;

        let mut receivers = WaitReceivers::default();
        if let Ok(result) = &mut result {
            // Only an input-only exit leaves nudges here; a delivered result took them.
            if !nudged_assignment_ids.is_empty() && result.nudged_assignment_ids.is_empty() {
                result
                    .message
                    .push_str(&nudge_sentence(&nudged_assignment_ids));
                result.nudged_assignment_ids = std::mem::take(&mut nudged_assignment_ids);
            }
            // Mail stays queued for the next request; name who it is from.
            let mut mailbox_authors = Vec::new();
            if result.mailbox_wake {
                for author in session.input_queue.pending_mailbox_authors().await {
                    if !mailbox_authors.contains(&author) {
                        mailbox_authors.push(author);
                    }
                }
            }
            if !mailbox_authors.is_empty() {
                let authors = mailbox_authors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                result.message = result
                    .message
                    .replacen(MAILBOX_WAKE_MESSAGE, &mailbox_wake_message(&authors), 1);
            }
            receivers = wait_receivers(&session, result, &mailbox_authors).await;
        }

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: if result.is_ok() {
                        CollabAgentToolCallStatus::Completed
                    } else {
                        CollabAgentToolCallStatus::Failed
                    },
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receivers.thread_ids,
                    receiver_agents: receivers.agents,
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: receivers.states,
                }),
            )
            .await;

        result.map(boxed_tool_output)
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
    hex::encode(Sha256::digest(value.as_bytes()))
}

#[derive(Clone, Default)]
struct HydratedAssignments {
    revisions: BTreeMap<AssignmentId, String>,
    tasks: BTreeMap<AssignmentId, AgentTask>,
    failures: BTreeMap<AssignmentId, String>,
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
        let page_assignment_ids = wake_assignment_ids(&page);
        if !page_assignment_ids
            .iter()
            .all(|assignment_id| hydrated_assignments.revisions.contains_key(assignment_id))
        {
            return WakePageAcceptance::FailOpen(
                "durable wake pagination omitted a task revision".to_string(),
            );
        }
        for (assignment_id, current) in &hydrated_assignments.revisions {
            let Some(accepted) = self.hydrated_assignments.revisions.get(assignment_id) else {
                continue;
            };
            if current != accepted {
                return WakePageAcceptance::FailOpen(format!(
                    "assignment {assignment_id} changed revision during durable wake pagination"
                ));
            }
        }

        self.seen_event_ids
            .extend(page.updated_agents.iter().map(|event| event.event_id));
        self.hydrated_assignments
            .revisions
            .extend(hydrated_assignments.revisions);
        self.hydrated_assignments
            .tasks
            .extend(hydrated_assignments.tasks);
        self.hydrated_assignments.failures.extend(hydrated_assignments.failures);
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
        self.hydrated_assignments.failures.is_empty() && self.combined.remaining_count > 0
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

    fn assignment_ids(&self) -> BTreeSet<AssignmentId> {
        self.hydrated_assignments
            .revisions
            .keys()
            .copied()
            .collect()
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
        let task = match coordinator
            .get_agent_task(*assignment_id, Some(0))
            .await
        {
            Ok(task) => task,
            Err(error) if assignment_local_failure(&error) => {
                // Stable sentinel is not task evidence. Keep this obligation
                // visible on every owner read, including after cursor advance.
                let revision = sha256_text(&format!("unavailable:{assignment_id}"));
                hydrated.revisions.insert(*assignment_id, revision);
                hydrated.failures.insert(*assignment_id, task_store_error_detail("wait_agent", error));
                continue;
            }
            Err(error) => return Err(task_store_error_detail("wait_agent", error)),
        };
        let revision = agent_task_revision(&task)
            .map_err(|error| format!("assignment {assignment_id} revision: {error}"))?;
        hydrated.revisions.insert(*assignment_id, revision);
        hydrated.tasks.insert(*assignment_id, task);
    }
    Ok(hydrated)
}

fn agent_task_revision(task: &AgentTask) -> Result<String, serde_json::Error> {
    serde_json::to_vec(task).map(|bytes| hex::encode(Sha256::digest(bytes)))
}

async fn hydrate_wait_owner_assignments(
    coordinator: &crate::agent::task_coordinator::AgentTaskCoordinator,
    store: Option<&std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>>,
    root_session_id: Option<&str>,
    consuming_agent_path: &str,
) -> Result<HydratedAssignments, String> {
    let (Some(store), Some(root_session_id)) = (store, root_session_id) else {
        return Ok(HydratedAssignments::default());
    };
    let descendant_prefix = format!("{consuming_agent_path}/");
    let assignment_ids = store
        .list_agent_task_bindings(root_session_id.to_string(), None)
        .await
        .map_err(|error| {
            format!(
                "wait_agent could not list its durable task owners: {}",
                task_store_error_detail("wait_agent", error)
            )
        })?
        .into_iter()
        .filter(|binding| binding.agent_path.starts_with(&descendant_prefix))
        .map(|binding| binding.assignment_id)
        .collect::<BTreeSet<_>>();
    hydrate_assignments(coordinator, &assignment_ids).await
}

fn wait_owner_is_settled(assignments: &HydratedAssignments) -> bool {
    assignments.failures.is_empty() && !assignments.tasks.is_empty()
        && assignments.tasks.values().all(|task| {
            task.current_attempt.state.is_terminal()
                && (task.current_attempt.state != AttemptState::Completed
                    || !has_pending_required_gates(task))
        })
}

fn has_pending_required_gates(task: &AgentTask) -> bool {
    matches!(
        task.assignment.admission_origin,
        AssignmentAdmissionOrigin::Typed
    ) && task
        .workspace_status
        .pending_gates
        .iter()
        .any(|gate| !gate.is_waivable())
}

enum BacklogPageRead {
    Page(codex_agent_task_store::StoreResult<WakeRead>),
    Activity(WaitOutcome),
    Cancelled,
}

async fn read_next_wake_page(
    store: &std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>,
    root_session_id: &str,
    cursor: WakeEventId,
    input: &mut AgentWaitInput<'_>,
    cancellation_token: &CancellationToken,
) -> BacklogPageRead {
    tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => BacklogPageRead::Cancelled,
        outcome = input.next() => BacklogPageRead::Activity(outcome),
        page = store.read_wake_events(root_session_id.to_string(), Some(cursor)) => BacklogPageRead::Page(page),
    }
}

struct AgentWaitInput<'a> {
    session: &'a crate::session::session::Session,
    turn_state: Option<&'a tokio::sync::Mutex<crate::state::TurnState>>,
    receiver: tokio::sync::watch::Receiver<InputQueueActivity>,
}

impl AgentWaitInput<'_> {
    async fn current(&mut self) -> Option<WaitOutcome> {
        self.receiver.borrow_and_update();
        self.session
            .input_queue
            .pending_agent_activity(self.turn_state)
            .await
            .map(input_outcome)
    }

    async fn next(&mut self) -> WaitOutcome {
        input_outcome(
            self.session
                .input_queue
                .next_agent_activity(self.turn_state, &mut self.receiver)
                .await,
        )
    }
}

fn input_outcome(activity: InputQueueActivity) -> WaitOutcome {
    match activity {
        InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
        InputQueueActivity::Steer => WaitOutcome::Steered,
        InputQueueActivity::InternalCompletion => WaitOutcome::DeferredCompletion,
    }
}

fn input_only_read(cursor: Option<WakeEventId>) -> WakeRead {
    WakeRead {
        status: WakeReadStatus::Unknown,
        reason: None,
        updated_agents: Vec::new(),
        latest_event_id: cursor,
        lost_to_retention_count: 0,
        remaining_count: 0,
        truncated_count: 0,
        timed_out: false,
    }
}

fn input_only_result(
    turn: &crate::session::turn_context::TurnContext,
    outcome: WaitOutcome,
    cursor: Option<WakeEventId>,
    started: Instant,
) -> WaitAgentResult {
    turn.session_telemetry.counter(
        "codex.multi_agent.root_wait",
        1,
        &[(
            "outcome",
            match outcome {
                WaitOutcome::Steered => "steered",
                WaitOutcome::MailboxActivity => "mailbox",
                _ => "deferred_completion",
            },
        )],
    );
    turn.session_telemetry.histogram(
        "codex.multi_agent.root_wait_duration_ms",
        i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
        &[],
    );
    // No durable page has been delivered. The cursor is unchanged (or
    // uninitialized), and no authoritative completion or delivery receipt exists.
    WaitAgentResult::from_outcome(
        outcome,
        cursor.map(|value| value.to_string()),
        Vec::new(),
        0,
        Vec::new(),
    )
}


impl CoreToolRuntime for Handler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }

    fn cancellation_requires_commit_barrier(&self) -> bool {
        // Let cooperative cancellation close the published item before TurnAborted.
        true
    }

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) previous_delivery: Option<JsonValue>,
    pub(crate) typed_deltas: Vec<JsonValue>,
    pub(crate) truncated_count: u64,
    pub(crate) nudged_assignment_ids: Vec<String>,
    #[serde(skip)]
    pub(crate) deterministic_continuation_receipts: Vec<TurnTimingDeterministicContinuationReceipt>,
    #[serde(skip)]
    pub(crate) authoritative_wait_signal: Option<JsonValue>,
    /// Mail ended this wait; the handler names its authors before returning.
    #[serde(skip)]
    pub(crate) mailbox_wake: bool,
}

/// Every message names its outcome kind. Mail is delivered with the next
/// request, so a mailbox wake is never a statement that agent work finished.
const MAILBOX_WAKE_MESSAGE: &str =
    "Wait woke on mailbox activity: new agent messages arrive with your next request.";

const SETTLED_OWNER_MESSAGE: &str = "Wait found no pending owned agent work: every assignment this wait owns has settled, so another wait returns immediately.";

fn mailbox_wake_message(authors: &[String]) -> String {
    format!(
        "Wait woke on mailbox activity: messages from {} arrive with your next request.",
        crate::session::turn_execution::bounded_path_list(authors)
    )
}

fn nudge_sentence(nudged_assignment_ids: &[String]) -> String {
    format!(
        " Sent the one allowed no-progress nudge to assignments [{}].",
        nudged_assignment_ids.join(", ")
    )
}

fn admission_fold_note(count: usize) -> String {
    format!(
        " Skipped {count} admission-only (accepted) event(s) without waking; admission is not progress."
    )
}

/// Admission, including a legacy follow-up's new attempt, is lifecycle
/// bookkeeping rather than progress (`ObservationKind::is_meaningful_progress`).
/// A page that lost events to retention still carries a deliverable signal.
fn is_admission_only(wake_read: &WakeRead) -> bool {
    wake_read.lost_to_retention_count == 0
        && !wake_read.updated_agents.is_empty()
        && wake_read
            .updated_agents
            .iter()
            .all(|event| event.reason == codex_agent_task_store::ObservationKind::Accepted)
}

#[derive(Default)]
struct WaitReceivers {
    thread_ids: Vec<codex_protocol::ThreadId>,
    agents: Vec<codex_protocol::protocol::CollabAgentRef>,
    states: HashMap<codex_protocol::ThreadId, AgentStatus>,
}

/// Agents whose durable deltas or mailbox messages this wait delivered, in
/// delivery order, with their current runtime states (as V1 wait reports).
async fn wait_receivers(
    session: &crate::session::session::Session,
    result: &WaitAgentResult,
    mailbox_authors: &[AgentPath],
) -> WaitReceivers {
    let agent_control = &session.services.agent_control;
    let coordinator = agent_control.task_coordinator();
    let mut thread_ids = Vec::new();
    let mut seen_threads = HashSet::new();
    let mut seen_assignments = BTreeSet::new();
    for delta in &result.typed_deltas {
        let thread_id = if let Some(thread_id) = delta.get("thread_id").and_then(JsonValue::as_str)
        {
            codex_protocol::ThreadId::from_string(thread_id).ok()
        } else if let Some(assignment_id) = delta
            .get("assignment_id")
            .and_then(JsonValue::as_str)
            .and_then(|value| AssignmentId::parse(value).ok())
        {
            if !seen_assignments.insert(assignment_id) {
                continue;
            }
            coordinator
                .get_agent_task_binding(assignment_id)
                .await
                .ok()
                .flatten()
                .and_then(|binding| binding.thread_id)
                .and_then(|value| codex_protocol::ThreadId::from_string(&value).ok())
        } else {
            None
        };
        if let Some(thread_id) = thread_id
            && seen_threads.insert(thread_id)
        {
            thread_ids.push(thread_id);
        }
    }
    for author in mailbox_authors {
        if let Some(thread_id) = agent_control.get_agent_id_for_path(author)
            && seen_threads.insert(thread_id)
        {
            thread_ids.push(thread_id);
        }
    }
    let mut receivers = WaitReceivers::default();
    for thread_id in thread_ids {
        let metadata = agent_control
            .get_agent_metadata(thread_id)
            .unwrap_or_default();
        receivers.agents.push(codex_protocol::protocol::CollabAgentRef {
            thread_id,
            agent_nickname: metadata.agent_nickname,
            agent_role: metadata.agent_role,
        });
        receivers
            .states
            .insert(thread_id, agent_control.get_status(thread_id).await);
        receivers.thread_ids.push(thread_id);
    }
    receivers
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
            WaitOutcome::MailboxActivity => MAILBOX_WAKE_MESSAGE,
            WaitOutcome::DeferredCompletion => {
                "A deferred MCP result is waiting in your next response."
            }
            WaitOutcome::DurableActivity => "Durable typed-task progress is available.",
            WaitOutcome::MaintenanceActivity => "Wait maintenance produced a state change.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::BoundaryElapsed => "Wait maintenance boundary elapsed.",
            WaitOutcome::TimedOut => "Wait timed out.",
        }
        .to_string();
        if !nudged_assignment_ids.is_empty() {
            message.push_str(&nudge_sentence(&nudged_assignment_ids));
        }
        Self {
            message,
            timed_out: outcome == WaitOutcome::TimedOut,
            cursor,
            previous_delivery: None,
            typed_deltas,
            truncated_count,
            nudged_assignment_ids,
            deterministic_continuation_receipts: Vec::new(),
            authoritative_wait_signal: None,
            mailbox_wake: outcome == WaitOutcome::MailboxActivity,
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
    if !assignments.failures.is_empty() {
        // Suppress automatic terminal delivery, not healthy sibling progress.
        return Some(Vec::new());
    }
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
        has_pending_gates: has_pending_required_gates(task),
        task_revision,
    }
}

fn authoritative_wait_signal(
    root_session_id: Option<&str>,
    consuming_agent_path: &str,
    states: &[AuthoritativeWaitState],
    surfaceable_message: Option<&str>,
) -> Option<JsonValue> {
    if states.is_empty()
        || states
            .iter()
            .any(|state| state.state == AttemptState::Completed && state.has_pending_gates)
    {
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
    let assignments = hydrate_wait_owner_assignments(
        coordinator,
        Some(&store),
        Some(&root_session_id),
        &consuming_agent_path,
    )
    .await
    .ok()?;
    // The saved revision describes the complete owner set, not just the last
    // delivered deltas. Never discard new recovery children before comparing it.
    if !guarded_assignment_ids
        .iter()
        .all(|id| assignments.tasks.contains_key(id))
    {
        return None;
    }
    let states = authoritative_wait_states(&assignments)?;
    authoritative_wait_snapshot(Some(&root_session_id), &consuming_agent_path, &states)
}

async fn nudge_stalled_assignments(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    store: Option<&std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>>,
    root_session_id: Option<&str>,
    turn_state: Option<&tokio::sync::Mutex<crate::state::TurnState>>,
) -> Vec<String> {
    let (Some(store), Some(root_session_id)) = (store, root_session_id) else {
        return Vec::new();
    };
    if let Err(error) = session
        .services
        .agent_control
        .task_coordinator()
        .recover_unstarted_assignments()
        .await
    {
        tracing::warn!(%error, "failed to reconcile interrupted task launches");
    }
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
        if session
            .input_queue
            .pending_agent_activity(turn_state)
            .await
            .is_some()
        {
            break;
        }
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
        if !runtime_status_allows_nonproductive_recovery(&status) {
            if let Ok(path) = AgentPath::try_from(binding.agent_path.as_str())
                && let Err(error) = session
                    .services
                    .agent_control
                    .task_coordinator()
                    .seal_missing_receipt_for_attempt(&path, thread_id, &status, binding.attempt_id)
                    .await
            {
                tracing::warn!(%error, "terminal receipt recovery remains pending");
            }
            continue;
        }
        if session
            .input_queue
            .pending_agent_activity(turn_state)
            .await
            .is_some()
        {
            break;
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
        // Finish any accepted recovery/nudge before observing input, but do
        // not admit a new mutation after the passive wait has been steered.
        if session
            .input_queue
            .pending_agent_activity(turn_state)
            .await
            .is_some()
        {
            break;
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
        let communication = stalled_assignment_nudge(author.clone(), receiver_path);
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

fn stalled_assignment_nudge(author: AgentPath, recipient: AgentPath) -> InterAgentCommunication {
    communication_from_plaintext_message(
        author,
        recipient,
        "Coordination nudge: no durable progress has been observed. Continue the assigned work if possible; if blocked, finish with one final report naming the blocker. Do not send progress messages or restart a stale validation loop."
            .to_string(),
    )
}

fn runtime_status_allows_nonproductive_recovery(status: &AgentStatus) -> bool {
    matches!(status, AgentStatus::NotFound) || !is_final(status)
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
        let mut metadata =
            crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(
                self, true,
            )?;
        if let Some(receipt) = &self.previous_delivery {
            metadata.essential_inline["previous_delivery"] = receipt.clone();
        }
        Some(metadata)
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
    /// A deferred MCP result is queued for the next request.
    DeferredCompletion,
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
    store: Option<&std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>>,
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
            status: WakeReadStatus::NoStream,
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
    store: Option<&std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>>,
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
// The explicit input, deadline, durable-store, and cursor owners document
// every wake source participating in this private wait state machine.
async fn wait_for_activity(
    input: &mut AgentWaitInput<'_>,
    boundary_deadline: Option<Instant>,
    store: Option<&std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>>,
    root_session_id: Option<&str>,
    cursor: Option<WakeEventId>,
) -> codex_agent_task_store::StoreResult<(WaitOutcome, WakeRead)> {
    if let Some(outcome) = input.current().await {
        return Ok((outcome, input_only_read(cursor)));
    }
    if boundary_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return tokio::select! {
            biased;
            outcome = input.next() => Ok((outcome, input_only_read(cursor))),
            current = read_wake_events(store, root_session_id, cursor) => {
                let current = current?;
                Ok(if current.updated_agents.is_empty() {
                    (WaitOutcome::BoundaryElapsed, current)
                } else {
                    (WaitOutcome::DurableActivity, current)
                })
            }
        };
    }
    let boundary = async {
        match boundary_deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        outcome = input.next() => Ok((outcome, input_only_read(cursor))),
        durable = wait_wake_events(store, root_session_id, cursor) => Ok((WaitOutcome::DurableActivity, durable?)),
        _ = boundary => {
            tokio::select! {
                biased;
                outcome = input.next() => Ok((outcome, input_only_read(cursor))),
                current = read_wake_events(store, root_session_id, cursor) => {
                    let current = current?;
                    Ok(if current.updated_agents.is_empty() {
                        (WaitOutcome::BoundaryElapsed, current)
                    } else {
                        (WaitOutcome::DurableActivity, current)
                    })
                }
            }
        }
    }
}
