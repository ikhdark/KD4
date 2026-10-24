use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ToolLifecycleBoundary;
use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::ToolLifecycleWakeReason;
use codex_protocol::protocol::TurnTimingToolCallSource;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::code_mode::CodeModeWaitHandler;
use crate::tools::code_mode::WAIT_TOOL_NAME;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolDispatchState;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::turn_timing::ToolCallTimingLineage;
use crate::turn_timing::TurnTimingState;

#[test]
fn tool_lifecycle_uses_one_clock_and_records_all_boundaries() {
    let turn_timing = Arc::new(TurnTimingState::default());
    turn_timing.mark_turn_started();
    // Accepted a minute before the turn clock started: a boundary stamped from
    // the tool's own instants instead of the turn clock would read >= 60 s.
    let accepted_at = tokio::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("monotonic clock is older than a minute");
    let timing =
        ToolDispatchTiming::new_with_turn_clock(Arc::clone(&turn_timing), accepted_at, false);
    timing.mark_first_poll();
    turn_timing.adjust_parallel_gate_waiters(1);
    timing.mark_parallel_gate_admitted();
    turn_timing.adjust_parallel_gate_waiters(-1);
    turn_timing.adjust_active_tools(1);
    timing.mark_handler_entry();
    timing.mark_exec_process_spawned();
    timing.mark_exec_process_exited();
    timing.increment_retry_count();
    timing.mark_handler_entry();
    timing.mark_handler_exit();
    turn_timing.adjust_active_tools(-1);
    timing.mark_output_collected();
    assert!(timing.mark_relay_enqueue());
    let execution_id = timing.execution_id().clone();
    assert!(timing.mark_relay_delivery(&execution_id));

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert!(snapshot.first_poll_at_ms.is_some());
    // The tool's own timing ends at delivery. The turn appends the next sample
    // start to the recorded call when it dispatches the following request.
    turn_timing.record_tool_dispatch_timing(
        "call-1",
        "shell_command",
        TurnTimingToolCallSource::Direct,
        ToolCallTimingLineage::default(),
        snapshot.clone(),
    );
    // Dispatch marks the current request row; a dispatch without a request
    // wait is an invalid transition, so open the following request first.
    drop(turn_timing.begin_model_request_wait());
    turn_timing.mark_model_request_dispatched();
    let recorded = turn_timing.complete_snapshot().protocol_timing();
    let boundaries = recorded
        .tool_calls
        .iter()
        .flat_map(|call| call.lifecycle_events.iter())
        .map(|event| event.boundary)
        .collect::<Vec<_>>();
    assert_eq!(
        boundaries,
        vec![
            ToolLifecycleBoundary::RequestCreated,
            ToolLifecycleBoundary::Admitted,
            ToolLifecycleBoundary::HandlerStart,
            ToolLifecycleBoundary::ProcessSpawn,
            ToolLifecycleBoundary::ProcessExit,
            ToolLifecycleBoundary::HandlerReturn,
            ToolLifecycleBoundary::RelayEnqueue,
            ToolLifecycleBoundary::RelayDelivery,
            ToolLifecycleBoundary::NextModelSampleStart,
        ]
    );
    let offsets = recorded
        .tool_calls
        .iter()
        .flat_map(|call| call.lifecycle_events.iter())
        .map(|event| event.at_ms)
        .collect::<Vec<_>>();
    assert!(
        offsets.iter().all(|at_ms| *at_ms < 60_000),
        "every boundary is an offset on the turn clock: {offsets:?}"
    );
    assert_eq!(snapshot.retry_count, 1);
    assert_eq!(snapshot.reentry_count, 1);
    assert_eq!(
        snapshot
            .lifecycle_events
            .iter()
            .find(|event| event.boundary == ToolLifecycleBoundary::Admitted)
            .expect("admission event")
            .context
            .parallel_gate_waiter_count,
        1
    );
    assert_eq!(turn_timing.lifecycle_context().relay_queue_depth, 0);
}

#[test]
fn live_gate_phases_clear_only_after_the_last_owner_resumes_or_cancels() {
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::HarnessPhase;
    for (start, end, phase) in [
        (
            ToolLifecycleBoundary::ResourceResolutionStart,
            ToolLifecycleBoundary::ResourceResolutionEnd,
            HarnessPhase::ResourceResolution,
        ),
        (
            ToolLifecycleBoundary::DiffTrackerWaitStart,
            ToolLifecycleBoundary::DiffTrackerWaitEnd,
            HarnessPhase::DiffTracker,
        ),
        (
            ToolLifecycleBoundary::WorkspaceGateWaitStart,
            ToolLifecycleBoundary::WorkspaceGateWaitEnd,
            HarnessPhase::WorkspaceGate,
        ),
        (
            ToolLifecycleBoundary::EvidenceTrackerWaitStart,
            ToolLifecycleBoundary::EvidenceTrackerWaitEnd,
            HarnessPhase::EvidenceTracker,
        ),
    ] {
        let turn = Arc::new(TurnTimingState::default());
        let (tx, rx) = async_channel::unbounded();
        turn.bind_live_phase_events("turn".into(), tx);
        let first = ToolDispatchTiming::new_with_turn_clock(
            Arc::clone(&turn),
            tokio::time::Instant::now(),
            false,
        );
        let second = ToolDispatchTiming::new_with_turn_clock(
            Arc::clone(&turn),
            tokio::time::Instant::now(),
            false,
        );
        first.record_boundary(start);
        second.record_boundary(start);
        let EventMsg::TurnPhaseChanged(event) = rx.try_recv().unwrap().msg else {
            panic!("phase");
        };
        assert_eq!(event.phases, vec![phase]);
        first.record_boundary(end);
        assert!(rx.try_recv().is_err(), "another owner remains blocked");
        second.record_outcome("cancelled");
        let EventMsg::TurnPhaseChanged(event) = rx.try_recv().unwrap().msg else {
            panic!("phase");
        };
        assert!(event.phases.is_empty());
        drop(second);
        drop(first);
        assert!(rx.try_recv().is_err(), "no duplicate wakeups");
    }
}

#[tokio::test(start_paused = true)]
async fn process_wait_records_timeout_wake_and_reentry() {
    let turn_timing = Arc::new(TurnTimingState::default());
    turn_timing.mark_turn_started();
    let timing =
        ToolDispatchTiming::new_with_turn_clock(turn_timing, tokio::time::Instant::now(), false);
    timing.mark_handler_entry();
    timing.mark_handler_entry();
    timing.increment_retry_count();
    timing.record_timer_wait(ToolLifecycleTimerWait {
        wait_kind: "owner_wait".to_string(),
        requested_timeout_ms: Some(60_000),
        effective_timeout_ms: Some(60_000),
        deadline_at_ms: timing.deadline_after_ms(60_000),
        wake_reason: ToolLifecycleWakeReason::Timeout,
        sequence: 0,
    });

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(snapshot.retry_count, 1);
    assert_eq!(snapshot.reentry_count, 1);
    assert_eq!(snapshot.timer_waits.len(), 1);
    assert_eq!(snapshot.timer_waits[0].sequence, 1);
    assert_eq!(snapshot.timer_waits[0].deadline_at_ms, Some(60_000));
    assert_eq!(
        snapshot.timer_waits[0].wake_reason,
        ToolLifecycleWakeReason::Timeout
    );
}

#[test]
fn repeated_wakes_toward_one_deadline_fold_into_one_counted_record() {
    let timing = ToolDispatchTiming::new(tokio::time::Instant::now(), false);
    let wait = |requested_timeout_ms, deadline_at_ms, wake_reason| ToolLifecycleTimerWait {
        wait_kind: "owner_output_wait".to_string(),
        requested_timeout_ms: Some(requested_timeout_ms),
        effective_timeout_ms: Some(requested_timeout_ms),
        deadline_at_ms: Some(deadline_at_ms),
        wake_reason,
        sequence: 0,
    };
    for remaining in (1_000..2_000).rev() {
        timing.record_timer_wait(wait(remaining, 48_252, ToolLifecycleWakeReason::Completed));
    }
    timing.record_timer_wait(wait(1, 48_252, ToolLifecycleWakeReason::Timeout));
    timing.record_timer_wait(wait(500, 49_000, ToolLifecycleWakeReason::Timeout));

    let waits = timing.snapshot(tokio::time::Instant::now()).timer_waits;
    assert_eq!(
        waits
            .iter()
            .map(|wait| (wait.requested_timeout_ms, wait.wake_reason, wait.sequence))
            .collect::<Vec<_>>(),
        vec![
            // The first wake keeps its full requested timeout; the sequence
            // advances past all 1,000 wakes it now covers.
            (Some(1_999), ToolLifecycleWakeReason::Completed, 1_000),
            (Some(1), ToolLifecycleWakeReason::Timeout, 1_001),
            (Some(500), ToolLifecycleWakeReason::Timeout, 1_002),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn dispatch_timing_separates_item_poll_gate_authorization_and_handler_boundaries() {
    let accepted_at = tokio::time::Instant::now();
    let timing = ToolDispatchTiming::new(accepted_at, /*eager*/ true);

    tokio::time::advance(std::time::Duration::from_millis(20)).await;
    timing.mark_first_poll();
    tokio::time::advance(std::time::Duration::from_millis(30)).await;
    timing.mark_parallel_gate_admitted();
    timing.record_authorization_state_coordination(std::time::Duration::from_millis(17));
    tokio::time::advance(std::time::Duration::from_millis(40)).await;
    timing.mark_handler_entry();
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    timing.mark_handler_exit();
    tokio::time::advance(std::time::Duration::from_millis(5)).await;
    timing.mark_output_collected();
    timing.record_workspace_evidence_before(std::time::Duration::from_millis(3));
    timing.record_workspace_evidence_before_attribution(
        false,
        vec!["worktree".to_string(), "untracked".to_string()],
    );
    timing.record_workspace_evidence_after(std::time::Duration::from_millis(4));

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert!(snapshot.eager);
    assert_eq!(snapshot.item_to_first_poll_ms, Some(20));
    assert_eq!(snapshot.parallel_gate_wait_ms, Some(30));
    assert_eq!(snapshot.authorization_state_coordination_ms, Some(17));
    assert_eq!(snapshot.first_poll_to_handler_entry_ms, Some(70));
    assert_eq!(snapshot.handler_duration_ms, Some(10));
    assert_eq!(snapshot.first_poll_to_output_collected_ms, Some(85));
    assert_eq!(snapshot.workspace_evidence_before_ms, Some(3));
    assert_eq!(snapshot.workspace_evidence_before_cache_hit, Some(false));
    assert_eq!(
        snapshot.workspace_evidence_before_timed_out_git_dependencies,
        vec!["worktree", "untracked"]
    );
    assert_eq!(snapshot.workspace_evidence_after_ms, Some(4));
    assert_eq!(snapshot.post_handler_ms, Some(5));
    assert_eq!(snapshot.total_duration_ms, Some(85));
}

#[tokio::test(start_paused = true)]
async fn retained_read_admission_excludes_validation_and_is_committed_only_on_replay() {
    let accepted = tokio::time::Instant::now();
    let timing = ToolDispatchTiming::new(accepted, false);
    timing.mark_first_poll();
    tokio::time::advance(std::time::Duration::from_millis(2)).await;
    let admission = timing.capture_gate_admission();
    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        timing
            .snapshot(tokio::time::Instant::now())
            .parallel_gate_wait_ms,
        None
    );
    timing.mark_captured_gate_admission(admission);
    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(snapshot.parallel_gate_wait_ms, Some(2));
    assert!(snapshot.parallel_gate_admitted);
    assert_eq!(snapshot.handler_duration_ms, None);
    assert_eq!(snapshot.exec_request_to_spawn_ms, None);

    let fallback = ToolDispatchTiming::new(tokio::time::Instant::now(), false);
    fallback.mark_first_poll();
    let _rejected_replay = fallback.capture_gate_admission();
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    fallback.mark_parallel_gate_admitted();
    assert_eq!(
        fallback
            .snapshot(tokio::time::Instant::now())
            .parallel_gate_wait_ms,
        Some(50)
    );
}

#[tokio::test(start_paused = true)]
async fn dispatch_timing_measures_exec_spawn_from_request_acceptance() {
    let accepted_at = tokio::time::Instant::now();
    let timing = ToolDispatchTiming::new(accepted_at, /*eager*/ false);

    tokio::time::advance(std::time::Duration::from_millis(20)).await;
    timing.mark_first_poll();
    tokio::time::advance(std::time::Duration::from_millis(30)).await;
    timing.mark_exec_process_spawned();

    let live_snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(live_snapshot.item_to_first_poll_ms, Some(20));
    assert_eq!(live_snapshot.exec_request_to_spawn_ms, Some(50));
    assert!(live_snapshot.exec_process_alive_at_delivery);

    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    timing.mark_exec_process_exited();
    timing.record_outcome("failure");
    timing.record_exec_cleanup_state(
        /*background_process_expected*/ false, /*running_process_after_cleanup*/ true,
    );
    tokio::time::advance(std::time::Duration::from_millis(7)).await;

    let exited_snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(exited_snapshot.exec_request_to_spawn_ms, Some(50));
    assert_eq!(exited_snapshot.exec_spawn_to_exit_ms, Some(100));
    assert_eq!(exited_snapshot.exec_exit_to_delivery_ms, Some(7));
    assert_eq!(exited_snapshot.exec_spawn_to_delivery_ms, Some(107));
    assert!(!exited_snapshot.exec_process_alive_at_delivery);
    assert_eq!(exited_snapshot.outcome, Some("failure"));
    assert!(exited_snapshot.exec_cleanup_state_observed);
    assert!(!exited_snapshot.exec_background_process_expected);
    assert!(exited_snapshot.exec_running_process_after_cleanup);
}

#[tokio::test(start_paused = true)]
async fn confirmed_abort_cleanup_closes_retained_process_lifecycle_before_handler_return() {
    let turn_timing = Arc::new(TurnTimingState::default());
    turn_timing.mark_turn_started();
    let timing = ToolDispatchTiming::new_with_turn_clock(
        turn_timing,
        tokio::time::Instant::now(),
        /*eager*/ false,
    );
    timing.mark_first_poll();
    timing.mark_parallel_gate_admitted();
    timing.mark_handler_entry();
    timing.mark_exec_process_spawned();

    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    timing.mark_exec_process_exited();
    timing.record_exec_cleanup_state(
        /*background_process_expected*/ false, /*running_process_after_cleanup*/ false,
    );
    timing.mark_handler_exit_if_entered();

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(snapshot.handler_duration_ms, Some(10));
    assert_eq!(snapshot.exec_spawn_to_exit_ms, Some(10));
    assert!(!snapshot.exec_process_alive_at_delivery);
    assert!(snapshot.exec_cleanup_state_observed);
    assert!(!snapshot.exec_background_process_expected);
    assert!(!snapshot.exec_running_process_after_cleanup);
    let process_exit = snapshot
        .lifecycle_events
        .iter()
        .position(|event| event.boundary == ToolLifecycleBoundary::ProcessExit)
        .expect("process exit boundary");
    let handler_return = snapshot
        .lifecycle_events
        .iter()
        .position(|event| event.boundary == ToolLifecycleBoundary::HandlerReturn)
        .expect("handler return boundary");
    assert!(process_exit < handler_return);
}

#[test]
fn relay_delivery_requires_enqueue_and_is_recorded_exactly_once() {
    let turn_timing = Arc::new(TurnTimingState::default());
    turn_timing.mark_turn_started();
    let timing =
        ToolDispatchTiming::new_with_turn_clock(turn_timing, tokio::time::Instant::now(), true);
    let execution_id = timing.execution_id().clone();
    let stale_execution_id = codex_protocol::protocol::ToolExecutionId("stale".to_string());

    assert!(!timing.mark_relay_delivery(&execution_id));
    assert!(timing.mark_relay_enqueue());
    assert!(!timing.mark_relay_delivery(&stale_execution_id));
    assert!(timing.mark_relay_delivery(&execution_id));
    assert!(!timing.mark_relay_delivery(&execution_id));

    assert_eq!(
        timing
            .snapshot(tokio::time::Instant::now())
            .lifecycle_events
            .iter()
            .filter(|event| event.boundary == ToolLifecycleBoundary::RelayDelivery)
            .count(),
        1
    );
}

#[test]
fn concurrent_relay_delivery_preserves_other_queued_tools() {
    for _ in 0..32 {
        let turn_timing = Arc::new(TurnTimingState::default());
        turn_timing.mark_turn_started();
        let delivered = ToolDispatchTiming::new_with_turn_clock(
            Arc::clone(&turn_timing),
            tokio::time::Instant::now(),
            false,
        );
        let pending = ToolDispatchTiming::new_with_turn_clock(
            Arc::clone(&turn_timing),
            tokio::time::Instant::now(),
            false,
        );
        assert!(delivered.mark_relay_enqueue());
        assert!(pending.mark_relay_enqueue());
        let ready = std::sync::Barrier::new(8);
        let accepted = std::thread::scope(|scope| {
            let attempts = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        ready.wait();
                        delivered.mark_relay_delivery(delivered.execution_id())
                    })
                })
                .collect::<Vec<_>>();
            attempts
                .into_iter()
                .map(|attempt| usize::from(attempt.join().expect("delivery thread")))
                .sum::<usize>()
        });
        assert_eq!(accepted, 1);
        assert_eq!(turn_timing.lifecycle_context().relay_queue_depth, 1);
        let snapshot = delivered.snapshot(tokio::time::Instant::now());
        assert_eq!(
            snapshot
                .lifecycle_events
                .iter()
                .filter(|event| event.boundary == ToolLifecycleBoundary::RelayDelivery)
                .count(),
            1
        );
        assert!(pending.mark_relay_delivery(pending.execution_id()));
        assert_eq!(turn_timing.lifecycle_context().relay_queue_depth, 0);
    }
}

