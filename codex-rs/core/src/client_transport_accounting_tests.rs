use super::*;
use codex_model_provider::BearerAuthProvider;

#[tokio::test]
async fn http_transport_start_duplicates_stay_idempotent_after_diagnostic_eviction() {
    let (_session, turn) = crate::session::tests::make_session_and_context().await;
    let timing = Arc::clone(&turn.turn_timing_state);
    timing.mark_turn_started();
    timing.begin_compaction_generation();
    let request = timing.begin_model_request_wait();
    let mut telemetry = ApiTelemetry::new(
        turn.session_telemetry,
        AuthRequestTelemetryContext::new(
            None,
            &BearerAuthProvider::for_test(None, None),
            None,
            PendingUnauthorizedRetry::default(),
        ),
        RequestRouteTelemetry::for_endpoint(RESPONSES_ENDPOINT),
        AuthEnvTelemetry::default(),
    );
    telemetry.model_request = Some(ModelRequestTelemetryContext {
        timing: Some(Arc::clone(&timing)),
        startup: None,
        identity: new_attempt_identity("sampling"),
        clock: ModelAttemptClock::new(),
        turn_id: None,
        retry_index: 0,
        dispatched: AtomicBool::new(false),
        highest_started_transport_attempt: StdMutex::new(None),
        readiness: StdMutex::new(None),
    });
    assert_eq!(
        timing
            .sampling_checkpoint()
            .timing
            .counters
            .model_request_count,
        0
    );
    // Exercise the production telemetry callback, which registers before counting.
    // More than the retained identity tail distinguishes it from row-only dedupe.
    for ordinal in 0..80 {
        telemetry.on_request_dispatched_attempt(ordinal);
        telemetry.on_request_dispatched_attempt(ordinal);
    }
    telemetry.on_request_dispatched_attempt(0);
    telemetry.on_request_dispatched_attempt(20);
    drop(request);
    let result = timing.complete_snapshot().protocol_timing();
    assert_eq!(result.counters.logical_generation_count, 1);
    assert_eq!(result.counters.model_request_count, 80);
    assert_eq!(result.counters.attempts_by_kind.primary, 1);
    assert_eq!(result.counters.attempts_by_kind.retry, 79);
    assert!(result.counters.physical_attempt_detail_eviction_count > 0);
}
