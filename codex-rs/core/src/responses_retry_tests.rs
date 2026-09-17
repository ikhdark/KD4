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
fn response_stream_retries_transport_timeouts() {
    assert!(should_retry_response_stream(&CodexErr::RequestTimeout));
}

#[test]
fn sampling_stream_error_keeps_its_outer_retry() {
    assert!(should_retry_response_stream(&CodexErr::Stream(
        "disconnected".to_string(),
        None
    )));
}

#[test]
fn transport_fallback_requires_a_transport_class_error() {
    assert!(should_switch_fallback_transport(&connection_failed()));
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
    assert!(!should_retry_response_stream(&unexpected_status(
        StatusCode::UNAUTHORIZED
    )));
    assert!(should_retry_response_stream(&unexpected_status(
        StatusCode::BAD_GATEWAY
    )));
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
        assert!(!should_retry_response_stream(&error), "status {status}");
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

    assert!(!should_retry_response_stream(&error));
}

#[tokio::test]
async fn server_requested_retry_delay_above_local_backoff_cap_is_preserved() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut client_session = session.services.model_client.new_session();
    let mut retry_state = ResponsesStreamRetryState::default();
    let cancellation_token = CancellationToken::new();
    let err = CodexErr::Stream("retry later".to_string(), Some(Duration::from_secs(60)));

    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let retry = handle_retryable_response_stream_error(
        &mut retry_state,
        5,
        err,
        &mut client_session,
        &session,
        &turn_context,
        ResponsesStreamRequest::Sampling,
        &cancellation_token,
    );
    tokio::pin!(retry);
    assert!(
        tokio::time::timeout(Duration::from_secs(59), retry.as_mut())
            .await
            .is_err(),
        "the request loop must not retry before the server's delay"
    );
    retry.await.expect("retry after the requested delay");
    assert!((Duration::from_secs(60)..=Duration::from_millis(60_001)).contains(&started.elapsed()));
    let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(emitted.iter().any(|event| matches!(&event.msg,
        EventMsg::StreamError(error)
            if error.message == "Reconnecting... 1/5 (next retry in 60.0s)")));
}

#[test]
fn local_retry_backoff_remains_bounded() {
    let err = CodexErr::Stream("retry later".to_string(), None);
    let first = response_stream_retry_delay(&err, 1);
    let second = response_stream_retry_delay(&err, 2);
    assert!(first > Duration::ZERO);
    assert!(second > first, "early retries must back off despite jitter");
    assert!(second < MAX_RESPONSE_STREAM_RETRY_DELAY);
    let saturated = response_stream_retry_delay(&err, 10);
    assert!(saturated >= MAX_RESPONSE_STREAM_RETRY_DELAY.mul_f64(0.9));
    assert!(saturated <= MAX_RESPONSE_STREAM_RETRY_DELAY);
}

#[test]
fn server_requested_retry_delay_below_the_ceiling_is_preserved() {
    let requested_delay = Duration::from_secs(2);
    let err = CodexErr::Stream("retry shortly".to_string(), Some(requested_delay));

    assert_eq!(response_stream_retry_delay(&err, 1), requested_delay);
}

#[tokio::test]
async fn exhausted_retry_budget_without_fallback_returns_the_error() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    assert!(!session.services.model_client.responses_websocket_enabled());
    let mut client_session = session.services.model_client.new_session();
    let mut retry_state = ResponsesStreamRetryState {
        retries: 5,
        ..Default::default()
    };
    let result = handle_retryable_response_stream_error(
        &mut retry_state,
        5,
        CodexErr::RequestTimeout,
        &mut client_session,
        &session,
        &turn_context,
        ResponsesStreamRequest::Sampling,
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(result, Err(CodexErr::RequestTimeout)));
    assert_eq!(retry_state.retries, 5);
    assert!(
        events.try_recv().is_err(),
        "exhaustion must not schedule or announce another retry"
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

#[tokio::test]
async fn connection_recovery_reaches_https_fallback_without_exhausting_network_waits() {
    let home = tempfile::tempdir().unwrap();
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |config| config.model_provider.supports_websockets = true,
        )
        .await;
    let mut client_session = session.services.model_client.new_session();
    let mut retry_state = ResponsesStreamRetryState::default();
    let cancellation_token = CancellationToken::new();
    assert!(session.services.model_client.responses_websocket_enabled());
    tokio::time::pause();

    for expected_delay in [Duration::from_secs(5), Duration::from_secs(10)] {
        let started = tokio::time::Instant::now();
        handle_retryable_response_stream_error(
            &mut retry_state,
            2,
            connection_failed(),
            &mut client_session,
            &session,
            &turn_context,
            ResponsesStreamRequest::Sampling,
            &cancellation_token,
        )
        .await
        .unwrap();
        assert!(
            (expected_delay..=expected_delay + Duration::from_millis(1))
                .contains(&started.elapsed())
        );
        assert_eq!(retry_state.retries, 0);
        assert!(session.services.model_client.responses_websocket_enabled());
    }

    let started = tokio::time::Instant::now();
    handle_retryable_response_stream_error(
        &mut retry_state,
        2,
        connection_failed(),
        &mut client_session,
        &session,
        &turn_context,
        ResponsesStreamRequest::Sampling,
        &cancellation_token,
    )
    .await
    .unwrap();
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(!session.services.model_client.responses_websocket_enabled());
    assert_eq!(retry_state.retries, 2);
    assert_eq!(retry_state.connection_retries, 2);
    let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(emitted.iter().any(|event| matches!(&event.msg,
        EventMsg::Warning(warning) if warning.message.contains("Falling back from WebSockets to HTTPS"))));
    assert!(emitted.iter().any(|event| matches!(&event.msg,
        EventMsg::StreamError(error) if error.message.contains("attempt 2, next retry in 10s"))));

    // A real network outage still waits after the transport fallback.
    let started = tokio::time::Instant::now();
    handle_retryable_response_stream_error(
        &mut retry_state,
        2,
        connection_failed(),
        &mut client_session,
        &session,
        &turn_context,
        ResponsesStreamRequest::Sampling,
        &cancellation_token,
    )
    .await
    .unwrap();
    assert!((Duration::from_secs(20)..=Duration::from_millis(20_001)).contains(&started.elapsed()));
    assert_eq!(retry_state.retries, 2);
    assert_eq!(retry_state.connection_retries, 3);

    let result = handle_retryable_response_stream_error(
        &mut retry_state,
        2,
        CodexErr::RequestTimeout,
        &mut client_session,
        &session,
        &turn_context,
        ResponsesStreamRequest::Sampling,
        &cancellation_token,
    )
    .await;
    assert!(matches!(result, Err(CodexErr::RequestTimeout)));
}
