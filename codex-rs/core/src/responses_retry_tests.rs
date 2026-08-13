use super::*;

#[test]
fn only_sampling_request_timeouts_skip_the_outer_retry() {
    let retry_decisions = [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ]
    .map(|request| should_retry_response_stream(request, &CodexErr::RequestTimeout));

    assert_eq!(retry_decisions, [false, true, true]);
}

#[test]
fn sampling_stream_error_keeps_its_outer_retry() {
    assert!(should_retry_response_stream(
        ResponsesStreamRequest::Sampling,
        &CodexErr::Stream("disconnected".to_string(), None)
    ));
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
