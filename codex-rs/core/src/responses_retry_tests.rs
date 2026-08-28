use super::*;
use codex_protocol::error::UnexpectedResponseError;

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
