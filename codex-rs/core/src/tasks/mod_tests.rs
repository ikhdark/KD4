use super::FINAL_PROOF_CANDIDATE_SEAL_TIMEOUT;
use super::TASK_COMPACT_METRIC;
use super::TERMINAL_MUTATION_FINALIZATION_TIMEOUT;
use super::TerminalDeadline;
use super::TerminalWaitError;
use super::apply_terminal_phase_timings_to_timing;
use super::emit_compact_metric;
use super::emit_turn_memory_metric;
use super::emit_turn_network_proxy_metric;
use super::merge_completion_review_partial;
use super::pending_terminal_recovery_state;
use super::protocol_terminalization_receipt;
use super::select_terminal_authority;
use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use crate::state::SamplingAdmission;
use crate::state::TerminalDeliveryState;
use crate::state::TerminalWakeResult;
use crate::state::TurnTerminalCoordinator;
use crate::task_evidence::TerminalDeliveryState as DurableDeliveryState;
use crate::task_evidence::TerminalRecoveryState;
use crate::task_evidence::TerminalizationReceiptSnapshot;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TerminalizationRecoveryState;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingTerminalization;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::Metric;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn test_session_telemetry() -> SessionTelemetry {
    let exporter = InMemoryMetricExporter::default();
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "codex-core", env!("CARGO_PKG_VERSION"), exporter)
            .with_runtime_reader(),
    )
    .expect("in-memory metrics client");
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.4",
        "gpt-5.4",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "tty".to_string(),
        SessionSource::Cli,
    )
    .with_metrics_without_metadata_tags(metrics)
}

fn find_metric<'a>(resource_metrics: &'a ResourceMetrics, name: &str) -> &'a Metric {
    for scope_metrics in resource_metrics.scope_metrics() {
        for metric in scope_metrics.metrics() {
            if metric.name() == name {
                return metric;
            }
        }
    }
    panic!("metric {name} missing");
}

fn attributes_to_map<'a>(
    attributes: impl Iterator<Item = &'a KeyValue>,
) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().to_string()))
        .collect()
}

fn metric_point(resource_metrics: &ResourceMetrics, name: &str) -> (BTreeMap<String, String>, u64) {
    let metric = find_metric(resource_metrics, name);
    match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => {
                let points: Vec<_> = sum.data_points().collect();
                assert_eq!(points.len(), 1);
                let point = points[0];
                (attributes_to_map(point.attributes()), point.value())
            }
            _ => panic!("unexpected counter aggregation"),
        },
        _ => panic!("unexpected counter data type"),
    }
}

#[test]
fn completion_review_partial_status_never_overrides_concrete_blockers() {
    for (initial, expected) in [
        (TaskCompletionStatus::Passed, TaskCompletionStatus::Partial),
        (TaskCompletionStatus::Partial, TaskCompletionStatus::Partial),
        (TaskCompletionStatus::Blocked, TaskCompletionStatus::Blocked),
    ] {
        let mut completion = Some(TaskCompletionGate {
            status: initial,
            reasons: vec!["task state".to_string()],
            evidence_path: None,
        });
        merge_completion_review_partial(&mut completion, vec!["review infrastructure".to_string()]);
        let completion = completion.expect("completion");
        assert_eq!(completion.status, expected);
        assert!(
            completion
                .reasons
                .contains(&"review infrastructure".to_string())
        );
    }

    let mut completion = None;
    merge_completion_review_partial(&mut completion, vec!["review infrastructure".to_string()]);
    assert_eq!(
        completion.expect("partial completion").status,
        TaskCompletionStatus::Partial
    );
}

#[tokio::test]
async fn abort_all_tasks_clears_empty_active_turn() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(session.active_turn.lock().await.is_none());
}

#[test]
fn final_proof_candidate_seal_timeout_is_dedicated() {
    assert_eq!(FINAL_PROOF_CANDIDATE_SEAL_TIMEOUT, Duration::from_secs(5));
    assert_eq!(
        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
        Duration::from_secs(1)
    );
}

#[tokio::test(start_paused = true)]
async fn final_proof_candidate_seal_can_outlive_generic_mutation_timeout() {
    let deadline = TerminalDeadline::start();

    let sealed = deadline
        .run(
            "final_proof_gate",
            FINAL_PROOF_CANDIDATE_SEAL_TIMEOUT,
            async {
                tokio::time::sleep(TERMINAL_MUTATION_FINALIZATION_TIMEOUT * 2).await;
                "sealed"
            },
        )
        .await;

    assert_eq!(sealed, Ok("sealed"));
}

#[test]
fn terminalization_recovery_states_project_to_public_receipts() {
    assert_eq!(
        pending_terminal_recovery_state(DurableDeliveryState::Delivered),
        TerminalRecoveryState::None
    );
    for delivery_state in [
        DurableDeliveryState::NotAttempted,
        DurableDeliveryState::Claimed,
        DurableDeliveryState::DeliveryFailed,
    ] {
        assert_eq!(
            pending_terminal_recovery_state(delivery_state),
            TerminalRecoveryState::Pending
        );
    }

    for (recovery_state, expected) in [
        (
            TerminalRecoveryState::None,
            TerminalizationRecoveryState::None,
        ),
        (
            TerminalRecoveryState::Pending,
            TerminalizationRecoveryState::Pending,
        ),
        (
            TerminalRecoveryState::Recovered,
            TerminalizationRecoveryState::Recovered,
        ),
    ] {
        let receipt = protocol_terminalization_receipt(TerminalizationReceiptSnapshot {
            terminal_identity: "thread:turn".to_string(),
            terminalization: TurnTimingTerminalization::default(),
            delivery_state: DurableDeliveryState::Delivered,
            active_turn_detached: true,
            terminal_interaction_released: true,
            recovery_state,
            deadline_exhausted_phase: None,
        });
        assert_eq!(receipt.recovery_state, expected);
    }
}

#[tokio::test]
async fn terminalization_recovery_notification_claim_is_one_shot() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let identity = format!("{}:turn", session.thread_id);
    assert!(
        session
            .register_authoritative_terminal_delivery(
                identity.clone(),
                "terminal-fingerprint".to_string(),
            )
            .await
    );

    assert!(
        session
            .claim_terminal_recovery_notification(&identity)
            .await
    );
    assert!(
        !session
            .claim_terminal_recovery_notification(&identity)
            .await
    );
}

#[tokio::test]
async fn taskless_placeholder_cleanup_is_pointer_identity_guarded() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let first = ActiveTurn::default();
    let first_state = Arc::clone(&first.turn_state);
    *session.active_turn.lock().await = Some(first);

    session.clear_taskless_placeholder(&first_state).await;
    assert!(session.active_turn.lock().await.is_none());

    let replacement = ActiveTurn::default();
    let replacement_state = Arc::clone(&replacement.turn_state);
    *session.active_turn.lock().await = Some(replacement);
    session.clear_taskless_placeholder(&first_state).await;

    let active = session.active_turn.lock().await;
    assert!(
        active.as_ref().is_some_and(|active_turn| {
            Arc::ptr_eq(&active_turn.turn_state, &replacement_state)
        })
    );
}

#[tokio::test(start_paused = true)]
async fn terminal_deadline_is_shared_and_starts_no_work_after_exhaustion() {
    let started = tokio::time::Instant::now();
    let deadline = TerminalDeadline::start();

    assert_eq!(
        deadline
            .run(
                "first_wait",
                Duration::from_secs(3),
                std::future::pending::<()>(),
            )
            .await,
        Err(TerminalWaitError::OperationTimedOut)
    );
    assert_eq!(
        deadline
            .run(
                "second_wait",
                Duration::from_secs(3),
                std::future::pending::<()>(),
            )
            .await,
        Err(TerminalWaitError::DeadlineExhausted)
    );
    assert_eq!(
        tokio::time::Instant::now().saturating_duration_since(started),
        Duration::from_secs(5)
    );
    assert_eq!(deadline.exhausted_phase().as_deref(), Some("second_wait"));

    let polled_after_expiry = Arc::new(AtomicBool::new(false));
    let future_polled = Arc::clone(&polled_after_expiry);
    assert_eq!(
        deadline
            .run("late_work", Duration::from_secs(1), async move {
                future_polled.store(true, Ordering::Release);
            })
            .await,
        Err(TerminalWaitError::DeadlineExhausted)
    );
    assert!(!polled_after_expiry.load(Ordering::Acquire));

    let timings = deadline.phase_timings_ns();
    assert_eq!(timings["first_wait"], 3_000_000_000);
    assert_eq!(timings["second_wait"], 2_000_000_000);
}

#[tokio::test]
async fn terminal_delivery_claim_is_one_shot_and_cleanup_is_a_separate_milestone() {
    let coordinator = TurnTerminalCoordinator::new("turn-1".to_string());
    let permit = coordinator.try_claim().expect("first terminal claimant");
    assert!(permit.mark_delivery_claimed());
    assert!(!permit.mark_delivery_claimed());
    permit.mark_delivery_attempted(false);
    assert_eq!(
        coordinator.delivery_state(),
        TerminalDeliveryState::DeliveryFailed
    );

    permit.mark_interaction_released();
    coordinator.wait_completed().await;
    let cleanup_waiter = {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move { coordinator.wait_cleanup_completed().await })
    };
    tokio::task::yield_now().await;
    assert!(!cleanup_waiter.is_finished());

    permit.complete_cleanup();
    cleanup_waiter.await.expect("cleanup waiter joins");
}

#[test]
fn cleanup_does_not_release_authoritative_turn_before_live_handoff() {
    let coordinator = TurnTerminalCoordinator::new("turn-pending-handoff".to_string());
    let permit = coordinator.try_claim().expect("terminal claimant");
    assert!(permit.mark_delivery_claimed());
    permit.mark_delivery_attempted(false);
    permit.complete_cleanup();

    assert!(!coordinator.interaction_released());
    assert_eq!(
        coordinator.delivery_state(),
        TerminalDeliveryState::DeliveryFailed
    );
}

#[tokio::test]
async fn interrupt_pending_fences_sampling_without_terminalizing() {
    let coordinator = TurnTerminalCoordinator::new("turn-fenced".to_string());
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Allowed);
    assert!(coordinator.mark_interrupt_pending().await);
    let generation = coordinator.wake_generation_id();
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Fenced);
    assert!(!coordinator.durable_terminal_committed());

    // The fence is pre-terminal: the normal coordinator can still claim the
    // eventual TurnAborted transition after output persistence.
    assert!(coordinator.try_claim().is_none());
    assert!(coordinator.interrupt_pending());
    assert_eq!(
        coordinator.mark_interrupt_output_durable(&generation),
        TerminalWakeResult::Applied
    );
    let permit = coordinator
        .try_claim()
        .expect("terminal claim opens after durability");
    permit.complete_cleanup();
    assert_eq!(
        coordinator.wait_for_interrupt_resolution(&generation).await,
        TerminalWakeResult::Applied
    );
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Allowed);
}

#[tokio::test]
async fn terminal_wakeups_are_generation_aware_and_waiter_counts_are_cancellation_safe() {
    let coordinator = TurnTerminalCoordinator::new("turn-generation".to_string());
    let stale_generation = coordinator.wake_generation_id();
    assert!(coordinator.mark_interrupt_pending().await);
    assert_eq!(
        coordinator
            .wait_for_interrupt_resolution(&stale_generation)
            .await,
        TerminalWakeResult::Stale
    );
    assert_eq!(
        coordinator.mark_interrupt_output_durable(&stale_generation),
        TerminalWakeResult::Stale
    );
    assert!(coordinator.try_claim().is_none());

    let generation = coordinator.wake_generation_id();
    let waiter = {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move { coordinator.wait_for_interrupt_resolution(&generation).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(coordinator.waiter_snapshot().interrupt_resolution, 1);
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(coordinator.waiter_snapshot().interrupt_resolution, 0);
}

#[test]
fn terminal_analytics_claim_is_process_local_one_shot_independent_of_delivery() {
    let coordinator = TurnTerminalCoordinator::new("turn-analytics".to_string());
    let permit = coordinator.try_claim().expect("terminal claimant");
    assert_eq!(
        coordinator.delivery_state(),
        TerminalDeliveryState::NotAttempted
    );
    assert!(permit.try_claim_analytics_emission());
    assert!(!permit.try_claim_analytics_emission());
    assert_eq!(
        coordinator.delivery_state(),
        TerminalDeliveryState::NotAttempted
    );
}

#[test]
fn terminal_analytics_claim_converges_across_in_process_recovery() {
    let before_claim = TurnTerminalCoordinator::new("turn-before-claim".to_string());
    drop(before_claim.try_claim().expect("normal terminal claimant"));
    let recovery = before_claim
        .try_claim()
        .expect("fail-safe terminal claimant");
    assert!(recovery.try_claim_analytics_emission());

    let after_claim = TurnTerminalCoordinator::new("turn-after-claim".to_string());
    let normal = after_claim.try_claim().expect("normal terminal claimant");
    assert!(normal.try_claim_analytics_emission());
    drop(normal);
    let recovery = after_claim
        .try_claim()
        .expect("fail-safe terminal claimant");
    assert!(!recovery.try_claim_analytics_emission());
}

#[test]
fn terminal_authority_selection_preserves_candidate_until_durable_recovery() {
    let (selected, durable) = select_terminal_authority(None, "exact candidate");
    assert_eq!(selected, "exact candidate");
    assert!(!durable);

    let (selected, durable) =
        select_terminal_authority(Some("durable authority"), "later candidate");
    assert_eq!(selected, "durable authority");
    assert!(durable);
}

#[test]
fn analytics_timing_snapshot_receives_all_final_terminal_phases() {
    let mut timing = TurnTiming::default();
    let phases = BTreeMap::from([
        ("delivery_attempt".to_string(), 11),
        ("interaction_release".to_string(), 13),
        ("post_cleanup".to_string(), 17),
        ("unclassified".to_string(), 19),
    ]);

    apply_terminal_phase_timings_to_timing(&mut timing, &phases);

    assert_eq!(timing.terminalization.delivery_attempt_ns, 11);
    assert_eq!(timing.terminalization.interaction_release_ns, 13);
    assert_eq!(timing.terminalization.post_cleanup_ns, 17);
    assert_eq!(timing.terminalization.unclassified_ns, 19);
}

#[test]
fn emit_turn_network_proxy_metric_records_active_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ true,
        ("tmp_mem_enabled", "true"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "true".to_string()),
            ("tmp_mem_enabled".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_network_proxy_metric_records_inactive_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ false,
        ("tmp_mem_enabled", "false"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "false".to_string()),
            ("tmp_mem_enabled".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_read_allowed_with_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ true,
        /*has_citations*/ true,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "true".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "true".to_string()),
            ("read_allowed".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_config_disabled_without_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ false,
        /*has_citations*/ false,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "false".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "false".to_string()),
            ("read_allowed".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn emit_compact_metric_records_manual_remote_v2() {
    let session_telemetry = test_session_telemetry();

    emit_compact_metric(&session_telemetry, "remote_v2", /*manual*/ true);

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TASK_COMPACT_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("manual".to_string(), "true".to_string()),
            ("type".to_string(), "remote_v2".to_string()),
        ])
    );
}

#[test]
fn emit_compact_metric_records_auto_local() {
    let session_telemetry = test_session_telemetry();

    emit_compact_metric(&session_telemetry, "local", /*manual*/ false);

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TASK_COMPACT_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("manual".to_string(), "false".to_string()),
            ("type".to_string(), "local".to_string()),
        ])
    );
}
