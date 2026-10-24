use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn both_response_queues_are_bound_to_the_actual_attempt() {
    let timing = Arc::new(TurnTimingState::default());
    timing.mark_turn_started();
    let request = timing.begin_model_request_wait();
    let identity = new_attempt_identity("queue-sampling");
    timing.record_model_attempt_identity(
        &identity.sampling_request_id,
        &identity.physical_attempt_id,
    );
    timing.mark_model_request_dispatched();
    let physical_attempt_id = identity.physical_attempt_id.clone();
    let mut attempt = ModelAttemptGuard::new(
        test_session_telemetry(),
        identity,
        0,
        ModelAttemptRetryReason::None,
        ModelAttemptRequestKind::Initial,
        ModelAttemptTransport::ResponsesHttp,
        None,
        ModelRequestMeasurements::default(),
        ModelAttemptClock::new(),
        None,
        None,
    );
    attempt.turn_timing = Some(Arc::clone(&timing));
    let (tx, rx_event) = codex_api::response_event_channel::response_event_channel(4);
    tx.send(Ok(ResponseEvent::Created)).await.unwrap();
    tx.send(Ok(ResponseEvent::Completed {
        response_id: "queue-response".into(),
        token_usage: None,
        end_turn: Some(true),
    }))
    .await
    .unwrap();
    drop(tx);
    let (mut stream, last_response) = super::super::map_response_stream(
        codex_api::ResponseStream {
            rx_event,
            upstream_request_id: None,
        },
        test_session_telemetry(),
        test_model_provider(),
        Some(attempt.into()),
        None,
        None,
    );
    let mut events = 0;
    while let Some(event) = stream.next().await {
        event.unwrap();
        events += 1;
    }
    assert_eq!(events, 2);
    assert_eq!(last_response.await.unwrap().response_id, "queue-response");
    drop(request);
    let profile = timing.complete_snapshot().protocol_timing();
    let queues = &profile.model_requests[0].response_queue_timing_by_attempt[&physical_attempt_id];
    assert_eq!(queues.len(), 2);
    for stage in ["api_to_mapper", "mapper_to_turn"] {
        let receipt = &queues[stage];
        assert_eq!((receipt.enqueued_events, receipt.dequeued_events), (2, 2));
        assert!(receipt.terminal_received);
    }
    assert!(profile.model_requests[0].token_usage.is_none());
}
