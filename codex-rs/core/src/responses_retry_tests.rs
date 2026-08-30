use super::*;
use codex_protocol::error::ConnectionFailedError;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::protocol::InternalSessionSource;

fn connection_failed() -> CodexErr {
    CodexErr::ConnectionFailed(ConnectionFailedError {
        message: "network is unreachable".to_string(),
        status: None,
    })
}

fn unexpected_status(status: StatusCode) -> CodexErr {
    CodexErr::UnexpectedStatus(UnexpectedResponseError {
        status,
        body: String::new(),
        user_message: None,
        url: None,
        cf_ray: None,
        request_id: None,
        identity_authorization_error: None,
        identity_error_code: None,
    })
}

#[test]
fn every_response_request_retries_transport_timeouts() {
    let retry_decisions = [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ]
    .map(|request| should_retry_response_stream(request, &CodexErr::RequestTimeout));

    assert_eq!(retry_decisions, [true, true, true]);
}

#[test]
fn sampling_stream_error_keeps_its_outer_retry() {
    assert!(should_retry_response_stream(
        ResponsesStreamRequest::Sampling,
        &CodexErr::Stream("disconnected".to_string(), None)
    ));
}

#[test]
fn transport_fallback_requires_a_transport_class_error() {
    assert!(should_switch_fallback_transport(&CodexErr::RequestTimeout));
    assert!(should_switch_fallback_transport(
        &CodexErr::ResponseStreamFailed(codex_protocol::error::ResponseStreamFailed {
            message: "websocket closed".to_string(),
            status: None,
            request_id: None,
        })
    ));
    assert!(!should_switch_fallback_transport(&CodexErr::Stream(
        "response.failed".to_string(),
        None,
    )));
    assert!(!should_switch_fallback_transport(
        &CodexErr::InternalServerError
    ));
}

#[test]
fn unauthorized_status_skips_every_outer_response_retry() {
    for request in [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
        assert!(!should_retry_response_stream(
            request,
            &unexpected_status(StatusCode::UNAUTHORIZED),
        ));
        assert!(should_retry_response_stream(
            request,
            &unexpected_status(StatusCode::BAD_GATEWAY),
        ));
    }
}

#[test]
fn deterministic_4xx_do_not_retry_or_fallback() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::METHOD_NOT_ALLOWED,
        StatusCode::UNPROCESSABLE_ENTITY,
    ] {
        let error = unexpected_status(status);
        assert!(
            !should_retry_response_stream(ResponsesStreamRequest::Sampling, &error),
            "status {status}"
        );
        assert!(!should_switch_fallback_transport(&error), "status {status}");
    }
}

#[test]
fn region_restricted_status_skips_every_outer_response_retry() {
    let error = CodexErr::RegionRestricted(UnexpectedResponseError {
        status: StatusCode::FORBIDDEN,
        body: "Cloudflare blocked".to_string(),
        user_message: Some("service unavailable in this region".to_string()),
        url: None,
        cf_ray: None,
        request_id: None,
        identity_authorization_error: None,
        identity_error_code: None,
    });

    for request in [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
        assert!(!should_retry_response_stream(request, &error));
    }
}

#[test]
fn server_requested_retry_delay_is_bounded() {
    let err = CodexErr::Stream("retry later".to_string(), Some(Duration::from_secs(60)));

    assert_eq!(
        response_stream_retry_delay(&err, 1),
        MAX_RESPONSE_STREAM_RETRY_DELAY
    );
}

#[test]
fn server_requested_retry_delay_below_the_ceiling_is_preserved() {
    let requested_delay = Duration::from_secs(2);
    let err = CodexErr::Stream("retry shortly".to_string(), Some(requested_delay));

    assert_eq!(response_stream_retry_delay(&err, 1), requested_delay);
}

#[test]
fn websocket_http_fallback_does_not_reset_the_retry_budget() {
    let mut retries = 0;

    exhaust_retry_budget_for_http_fallback(&mut retries, 5);

    assert_eq!(retries, 5);
    assert!(
        retries >= 5,
        "a failed HTTPS probe must not open a new retry window"
    );
}

#[tokio::test]
async fn retry_backoff_is_cancelled_by_owner() {
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();

    let result = wait_for_retry_delay(Duration::from_secs(60), &cancellation_token).await;

    assert!(matches!(result, Err(CodexErr::TurnAborted)));
}

#[test]
fn lost_connection_on_a_sampling_turn_waits_instead_of_spending_the_retry_budget() {
    assert!(should_wait_for_connection_recovery(
        ResponsesStreamRequest::Sampling,
        &connection_failed(),
        &SessionSource::VSCode,
        &ModelProviderInfo::default(),
    ));
}

#[test]
fn connection_recovery_wait_is_limited_to_user_facing_sampling_turns() {
    // Compaction requests stay on the bounded budget so they cannot stall a turn.
    for request in [
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
        assert!(!should_wait_for_connection_recovery(
            request,
            &connection_failed(),
            &SessionSource::VSCode,
            &ModelProviderInfo::default(),
        ));
    }

    // Internal sessions must fail fast for their callers.
    assert!(!should_wait_for_connection_recovery(
        ResponsesStreamRequest::Sampling,
        &connection_failed(),
        &SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
        &ModelProviderInfo::default(),
    ));

    // Bedrock reports unrelated failures through the same error class.
    assert!(!should_wait_for_connection_recovery(
        ResponsesStreamRequest::Sampling,
        &connection_failed(),
        &SessionSource::VSCode,
        &ModelProviderInfo::create_amazon_bedrock_provider(None),
    ));

    // Non-connection transport errors keep the bounded retry path.
    assert!(!should_wait_for_connection_recovery(
        ResponsesStreamRequest::Sampling,
        &CodexErr::RequestTimeout,
        &SessionSource::VSCode,
        &ModelProviderInfo::default(),
    ));
}

#[test]
fn connection_retry_delay_backs_off_and_is_bounded() {
    let initial = ResponsesStreamRetryState::default().connection_retry_delay;
    assert_eq!(initial, INITIAL_CONNECTION_RETRY_DELAY);

    assert_eq!(next_connection_retry_delay(initial), initial * 2);
    assert_eq!(
        next_connection_retry_delay(MAX_CONNECTION_RETRY_DELAY),
        MAX_CONNECTION_RETRY_DELAY
    );

    let mut delay = initial;
    for _ in 0..16 {
        delay = next_connection_retry_delay(delay);
    }
    assert_eq!(delay, MAX_CONNECTION_RETRY_DELAY);
}
