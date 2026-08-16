use super::TASK_COMPACT_METRIC;
use super::TerminalDeadline;
use super::TerminalWaitError;
use super::apply_terminal_phase_timings_to_timing;
use super::emit_compact_metric;
use super::emit_turn_memory_metric;
use super::emit_turn_network_proxy_metric;
use super::merge_completion_review_partial;
use super::terminal_final_proof_requires_sealing;
use super::terminal_wait_failure_reason;
use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use crate::state::SamplingAdmission;
use crate::state::TerminalDeliveryState;
use crate::state::TurnTerminalCoordinator;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnTiming;
use codex_tools::ToolFailureClass;
use codex_tools::ToolFailureDiagnostic;
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
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Fenced);
    assert!(!coordinator.durable_terminal_committed());

    // The fence is pre-terminal: the normal coordinator can still claim the
    // eventual TurnAborted transition after output persistence.
    assert!(coordinator.try_claim().is_none());
    assert!(coordinator.interrupt_pending());
    coordinator.mark_interrupt_output_durable();
    let permit = coordinator
        .try_claim()
        .expect("terminal claim opens after durability");
    permit.complete_cleanup();
    coordinator.wait_for_interrupt_resolution().await;
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Allowed);
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
#[test]
fn terminal_final_proof_sealing_is_reserved_for_mutating_tasks() {
    assert!(!terminal_final_proof_requires_sealing(false));
    assert!(terminal_final_proof_requires_sealing(true));
}

#[tokio::test(start_paused = true)]
async fn terminal_remaining_wait_uses_all_unspent_shared_budget() {
    let started = tokio::time::Instant::now();
    let deadline = TerminalDeadline::start();

    assert_eq!(
        deadline
            .run(
                "setup",
                Duration::from_secs(2),
                std::future::pending::<()>(),
            )
            .await,
        Err(TerminalWaitError::OperationTimedOut)
    );
    assert_eq!(
        deadline
            .run_remaining("final_proof_gate", std::future::pending::<()>())
            .await,
        Err(TerminalWaitError::DeadlineExhausted)
    );
    assert_eq!(
        tokio::time::Instant::now().saturating_duration_since(started),
        Duration::from_secs(5)
    );
}

#[test]
fn terminal_wait_failure_reasons_identify_phase_and_timeout_kind() {
    let operation_timeout: ToolFailureDiagnostic = serde_json::from_str(
        &terminal_wait_failure_reason("final_proof_gate", TerminalWaitError::OperationTimedOut),
    )
    .expect("operation timeout should be a structured completion diagnostic");
    assert_eq!(operation_timeout.class, ToolFailureClass::Completion);
    assert_eq!(
        operation_timeout.fingerprint,
        "completion.final_proof_gate.operation_timed_out"
    );
    assert!(operation_timeout.retryable);

    let exhausted: ToolFailureDiagnostic = serde_json::from_str(&terminal_wait_failure_reason(
        "final_proof_gate",
        TerminalWaitError::DeadlineExhausted,
    ))
    .expect("deadline exhaustion should be a structured completion diagnostic");
    assert_eq!(exhausted.class, ToolFailureClass::Completion);
    assert_eq!(
        exhausted.fingerprint,
        "completion.final_proof_gate.deadline_exhausted"
    );
    assert!(!exhausted.retryable);
}
