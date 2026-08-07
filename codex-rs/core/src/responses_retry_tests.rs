use super::*;

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
