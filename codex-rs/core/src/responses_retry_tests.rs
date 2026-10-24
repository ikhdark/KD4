use super::*;
use codex_protocol::error::ConnectionFailedError;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::protocol::InternalSessionSource;

#[test]
fn header_auth_account_switches_and_opaque_credential_scopes_have_recovery_identities() {
    let auth = |account: Option<&str>, credential: &str| {
        let mut headers = http::HeaderMap::new();
        headers.insert("authorization", credential.parse().unwrap());
        if let Some(account) = account {
            headers.insert("chatgpt-account-id", account.parse().unwrap());
        }
        codex_login::CodexAuth::Headers(codex_login::AuthHeaders::new(headers))
    };
    let identity = |account, key| AccountChangeIdentity::from_auth(&auth(account, key));
    assert!(identity(Some("a"), "first") == identity(Some("a"), "refreshed"));
    assert!(identity(Some("a"), "first") != identity(Some("b"), "second"));
    assert!(identity(None, "first") != identity(None, "second"));
    assert!(identity(None, "first").is_some());
}

fn connection_failed() -> CodexErr {
    CodexErr::ConnectionFailed(ConnectionFailedError {
        message: "network is unreachable".to_string(),
        status: None,
    })
}

fn unexpected_status(status: StatusCode) -> CodexErr {
    CodexErr::UnexpectedStatus(UnexpectedResponseError {
        retry_after: None,
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

#[tokio::test]
async fn unknown_dispatch_and_invalid_output_never_authorize_outer_retry_or_fallback() {
    for request in [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
        for cancelled in [false, true] {
            for error in [
                codex_api::ApiError::ProviderFailure {
                    code: Some("dispatch_outcome_unknown".into()),
                    message: "accepted request, acknowledgement lost".into(),
                },
                codex_api::ApiError::invalid_response("output fragment without an active item"),
            ] {
                let home = tempfile::tempdir().unwrap();
                let (session, turn, events) =
                    crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                        codex_login::CodexAuth::from_api_key("test key"),
                        Vec::new(),
                        home.path(),
                        |config| config.model_provider.supports_websockets = true,
                    )
                    .await;
                let mut client = session.services.model_client.new_session();
                let mut state = ResponsesStreamRetryState::default();
                let cancellation = CancellationToken::new();
                if cancelled {
                    cancellation.cancel();
                }
                let result = handle_retryable_response_stream_error(
                    &mut state,
                    5,
                    codex_api::map_api_error(error),
                    &mut client,
                    &session,
                    &turn,
                    request,
                    &cancellation,
                )
                .await;
                assert!(result.is_err());
                assert_eq!(matches!(result, Err(CodexErr::TurnAborted)), cancelled);
                assert_eq!(state.retries, 0);
                assert!(session.services.model_client.responses_websocket_enabled());
                assert!(
                    events.try_recv().is_err(),
                    "no retry/fallback may be announced"
                );
            }
        }
    }
}

#[test]
fn response_stream_retries_transport_timeouts() {
    assert!(should_retry_response_stream(&CodexErr::RequestTimeout));
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
            idle_timeout: false,
            reconnectable_close: false,
        })
    ));
    assert!(!should_switch_fallback_transport(&CodexErr::Stream(
        "response.failed".to_string(),
        None,
    )));
    assert!(!should_switch_fallback_transport(
        &CodexErr::InternalServerError { retry_after: None }
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
        retry_after: None,
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
async fn first_stream_read_failure_switches_to_https_with_bounded_remaining_retries() {
    for request in [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
        for max_retries in [0, 2] {
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

            let started = tokio::time::Instant::now();
            handle_retryable_response_stream_error(
                &mut retry_state,
                max_retries,
                codex_api::map_api_error(codex_api::ApiError::Stream(
                    "websocket closed before response.completed".to_string(),
                )),
                &mut client_session,
                &session,
                &turn_context,
                request,
                &cancellation_token,
            )
            .await
            .expect("retry immediately over HTTPS, not the failed WebSocket transport");
            assert_eq!(started.elapsed(), Duration::ZERO);
            assert!(!session.services.model_client.responses_websocket_enabled());
            assert_eq!(retry_state.retries, 0);
            let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(emitted.len(), 1);
            assert!(matches!(&emitted[0].msg, EventMsg::Warning(warning)
                if warning.message.contains("Falling back from WebSockets to HTTPS")));

            // HTTP failures consume the original budget and cannot activate fallback again.
            for expected_retries in 1..=max_retries {
                handle_retryable_response_stream_error(
                    &mut retry_state,
                    max_retries,
                    codex_api::map_api_error(codex_api::ApiError::Stream(
                        "stream closed before response.completed".to_string(),
                    )),
                    &mut client_session,
                    &session,
                    &turn_context,
                    request,
                    &cancellation_token,
                )
                .await
                .expect("remaining HTTP retry");
                assert_eq!(retry_state.retries, expected_retries);
                let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
                assert_eq!(emitted.len(), 1);
                assert!(matches!(&emitted[0].msg, EventMsg::StreamError(_)));
            }
            let result = handle_retryable_response_stream_error(
                &mut retry_state,
                max_retries,
                codex_api::map_api_error(codex_api::ApiError::Stream(
                    "stream closed before response.completed".to_string(),
                )),
                &mut client_session,
                &session,
                &turn_context,
                request,
                &cancellation_token,
            )
            .await;
            assert!(matches!(result, Err(CodexErr::ResponseStreamFailed(_))));
            assert_eq!(retry_state.retries, max_retries);
            assert!(events.try_recv().is_err());
            tokio::time::resume();
        }
    }
}

/// A server close that invites another connection must rebuild the WebSocket
/// rather than abandon it: falling back on the first such close degraded every
/// later request of the session, which the recorded rollouts showed happening
/// within the opening minute of long sessions.
#[tokio::test]
async fn reconnectable_server_close_rebuilds_the_websocket_before_falling_back() {
    for request in [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
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

        let reconnectable = || {
            codex_api::map_api_error(codex_api::ApiError::reconnectable_close(
                "websocket closed by server before response.completed (code 1012, reason: \"\")",
            ))
        };

        // The first close reconnects: the transport stays on WebSockets and the
        // provider retry budget is untouched.
        let started = tokio::time::Instant::now();
        handle_retryable_response_stream_error(
            &mut retry_state,
            /*max_retries*/ 2,
            reconnectable(),
            &mut client_session,
            &session,
            &turn_context,
            request,
            &cancellation_token,
        )
        .await
        .expect("a reconnectable close must retry");
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "the server already answered; there is nothing to back off for"
        );
        assert!(
            session.services.model_client.responses_websocket_enabled(),
            "one reconnectable close must not abandon WebSockets for the session"
        );
        assert_eq!(
            retry_state.retries, 0,
            "a reconnect must not spend the provider retry budget"
        );
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(emitted.len(), 1);
        assert!(
            matches!(&emitted[0].msg, EventMsg::StreamError(error)
                if error.message.contains("the server closed the stream")),
            "{:?}",
            emitted[0].msg
        );

        // A second close of the same request falls back, so a server closing in
        // a loop cannot reconnect without bound.
        handle_retryable_response_stream_error(
            &mut retry_state,
            /*max_retries*/ 2,
            reconnectable(),
            &mut client_session,
            &session,
            &turn_context,
            request,
            &cancellation_token,
        )
        .await
        .expect("the second close falls back to HTTPS");
        assert!(
            !session.services.model_client.responses_websocket_enabled(),
            "the reconnect is allowed once per request"
        );
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(emitted.len(), 1);
        assert!(matches!(&emitted[0].msg, EventMsg::Warning(warning)
            if warning.message.contains("Falling back from WebSockets to HTTPS")));
        tokio::time::resume();
    }
}

/// A close that rejects the request must keep failing fast: reconnecting would
/// only repeat the rejection.
#[test]
fn only_reconnectable_closes_rebuild_the_transport() {
    let close = |reconnectable_close| {
        CodexErr::ResponseStreamFailed(codex_protocol::error::ResponseStreamFailed {
            message: "websocket closed by server before response.completed".to_string(),
            status: None,
            request_id: None,
            idle_timeout: false,
            reconnectable_close,
        })
    };
    let fresh = ResponsesStreamRetryState::default();
    assert!(should_reconnect_after_close(&close(true), &fresh));
    assert!(!should_reconnect_after_close(&close(false), &fresh));
    assert!(!should_reconnect_after_close(&connection_failed(), &fresh));
    assert!(!should_reconnect_after_close(
        &CodexErr::RequestTimeout,
        &fresh
    ));

    let spent = ResponsesStreamRetryState {
        reconnected_after_close: true,
        ..ResponsesStreamRetryState::default()
    };
    assert!(!should_reconnect_after_close(&close(true), &spent));
}

#[tokio::test]
async fn server_requested_retry_delay_above_local_backoff_cap_is_preserved() {
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
    tokio::time::pause();
    let err = CodexErr::Stream(
        "retry later".to_string(),
        RetryAfter::from_delay(Duration::from_secs(60)),
    );
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
    assert!(session.services.model_client.responses_websocket_enabled());
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

#[tokio::test(start_paused = true)]
async fn server_requested_retry_delay_below_the_ceiling_is_preserved() {
    let requested_delay = Duration::from_secs(2);
    let err = CodexErr::Stream(
        "retry shortly".to_string(),
        RetryAfter::from_delay(requested_delay),
    );

    assert_eq!(response_stream_retry_delay(&err, 1), requested_delay);
    tokio::time::advance(requested_delay).await;
    assert_eq!(response_stream_retry_delay(&err, 1), Duration::ZERO);
}

#[tokio::test]
async fn exhausted_retry_budget_without_fallback_returns_the_error() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    session
        .services
        .model_client
        .force_http_fallback(&turn_context.session_telemetry);
    assert!(!session.services.model_client.responses_websocket_enabled());
    let mut client_session = session.services.model_client.new_session();
    let mut retry_state = ResponsesStreamRetryState {
        retries: 5,
        ..Default::default()
    };
    let advice = RetryAfter::from_delay(Duration::from_secs(30)).unwrap();
    let result = handle_retryable_response_stream_error(
        &mut retry_state,
        5,
        CodexErr::InternalServerError {
            retry_after: Some(advice),
        },
        &mut client_session,
        &session,
        &turn_context,
        ResponsesStreamRequest::Sampling,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(result.unwrap_err().retry_after(), Some(advice));
    assert_eq!(retry_state.retries, 5);
    assert!(
        events.try_recv().is_err(),
        "exhaustion must not schedule or announce another retry"
    );
}

#[tokio::test]
async fn transport_fallback_honors_the_original_retry_after_deadline() {
    let home = tempfile::tempdir().unwrap();
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |config| config.model_provider.supports_websockets = true,
        )
        .await;
    let mut client_session = session.services.model_client.new_session();
    let mut retry_state = ResponsesStreamRetryState::default();
    turn_context.turn_timing_state.mark_turn_started();
    tokio::time::pause();
    let advice = RetryAfter::from_delay(Duration::from_secs(10)).unwrap();
    let error = unexpected_status(StatusCode::SERVICE_UNAVAILABLE).with_retry_after(Some(advice));
    tokio::time::advance(Duration::from_secs(4)).await;
    let started = tokio::time::Instant::now();
    handle_retryable_response_stream_error(
        &mut retry_state,
        0,
        error,
        &mut client_session,
        &session,
        &turn_context,
        ResponsesStreamRequest::Sampling,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    // Tokio rounds timer deadlines to its millisecond tick; never retry early
    // or restart the original ten-second delay during transport fallback.
    assert!(
        (Duration::from_secs(6)..=Duration::from_millis(6_001)).contains(&started.elapsed()),
        "fallback waited {:?}",
        started.elapsed()
    );
    assert!(!session.services.model_client.responses_websocket_enabled());
    assert!(
        turn_context
            .turn_timing_state
            .complete_snapshot()
            .profile
            .exclusive
            .retry_only_ns
            > 0,
        "fallback Retry-After waiting must be attributed to retry backoff"
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
fn lost_connection_on_a_user_facing_turn_waits_instead_of_spending_the_retry_budget() {
    assert!(should_wait_for_connection_recovery(
        &connection_failed(),
        &SessionSource::VSCode,
        &ModelProviderInfo::default(),
    ));
}

#[test]
fn connection_recovery_wait_is_limited_to_user_facing_sessions() {
    // Internal sessions must fail fast for their callers.
    assert!(!should_wait_for_connection_recovery(
        &connection_failed(),
        &SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
        &ModelProviderInfo::default(),
    ));

    // Bedrock reports unrelated failures through the same error class.
    assert!(!should_wait_for_connection_recovery(
        &connection_failed(),
        &SessionSource::VSCode,
        &ModelProviderInfo::create_amazon_bedrock_provider(None),
    ));

    // Non-connection transport errors keep the bounded retry path.
    assert!(!should_wait_for_connection_recovery(
        &CodexErr::RequestTimeout,
        &SessionSource::VSCode,
        &ModelProviderInfo::default(),
    ));
}

#[test]
fn provider_silence_and_broken_streams_use_bounded_recovery() {
    // Silence after dispatch may hide provider work, not just an offline link.
    for message in [
        "idle timeout waiting for SSE after 300000ms",
        "idle timeout waiting for websocket after 300000ms",
        "model progress timeout waiting for SSE",
        "model progress timeout waiting for websocket",
    ] {
        let err = codex_api::map_api_error(codex_api::ApiError::Stream(message.to_string()));
        assert!(
            matches!(&err, CodexErr::ResponseStreamFailed(failed) if failed.idle_timeout),
            "{message} must be classified as provider silence"
        );
        assert!(
            !should_wait_for_connection_recovery(
                &err,
                &SessionSource::VSCode,
                &ModelProviderInfo::default(),
            ),
            "{message} must spend the bounded retry budget"
        );
    }

    // A stream that actually broke is not recoverable by waiting, so it keeps
    // the bounded retry path and its fast transport fallback.
    for message in [
        "websocket closed",
        "websocket connection is closed",
        "failed to encode responses request: boom",
    ] {
        let err = codex_api::map_api_error(codex_api::ApiError::Stream(message.to_string()));
        assert!(
            matches!(&err, CodexErr::ResponseStreamFailed(failed) if !failed.idle_timeout),
            "{message} must not be classified as provider silence"
        );
        assert!(
            !should_wait_for_connection_recovery(
                &err,
                &SessionSource::VSCode,
                &ModelProviderInfo::default(),
            ),
            "{message} must keep the bounded retry path"
        );
        // The transport fallback must still fire on the first such failure.
        assert!(should_switch_fallback_transport(&err), "{message}");
    }
}

#[tokio::test]
async fn repeated_http_idle_timeouts_exhaust_the_generation_budget() {
    for max_retries in [0, 2] {
        let (session, mut turn, _events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        std::sync::Arc::get_mut(&mut turn).unwrap().session_source = SessionSource::VSCode;
        session
            .services
            .model_client
            .force_http_fallback(&turn.session_telemetry);
        let mut client = session.services.model_client.new_session();
        let mut state = ResponsesStreamRetryState::default();
        tokio::time::pause();
        for attempt in 0..=max_retries {
            let error = codex_api::map_api_error(codex_api::ApiError::Stream(
                "idle timeout waiting for SSE after 100ms".into(),
            ));
            let result = handle_retryable_response_stream_error(
                &mut state,
                max_retries,
                error,
                &mut client,
                &session,
                &turn,
                ResponsesStreamRequest::Sampling,
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(result.is_ok(), attempt < max_retries);
        }
        assert_eq!(state.retries, max_retries);
        assert_eq!(state.connection_retries, 0);
        tokio::time::resume();
    }
}

#[tokio::test]
async fn compaction_waits_out_an_outage_longer_than_its_retry_budget() {
    // Remote compaction allows two retries. Spent on quick backoff, they once
    // covered about 0.6s of an outage that sampling turns waited out.
    for request in [
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
        let home = tempfile::tempdir().unwrap();
        let (session, mut turn_context, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test key"),
                Vec::new(),
                home.path(),
                |config| config.model_provider.supports_websockets = false,
            )
            .await;
        assert!(!session.services.model_client.responses_websocket_enabled());
        std::sync::Arc::get_mut(&mut turn_context)
            .expect("test turn context should be uniquely owned")
            .session_source = SessionSource::Cli;
        let mut client_session = session.services.model_client.new_session();
        let mut retry_state = ResponsesStreamRetryState::default();
        let cancellation_token = CancellationToken::new();
        tokio::time::pause();

        let started = tokio::time::Instant::now();
        for _ in 0..4 {
            handle_retryable_response_stream_error(
                &mut retry_state,
                2,
                connection_failed(),
                &mut client_session,
                &session,
                &turn_context,
                request,
                &cancellation_token,
            )
            .await
            .expect("compaction should wait for the network instead of failing the turn");
        }
        assert_eq!(retry_state.retries, 0, "{request:?}");
        assert_eq!(retry_state.connection_retries, 4, "{request:?}");
        // 0.5s + 1s + 2s + 4s, allowing Tokio's millisecond timer rounding.
        assert!(
            (Duration::from_millis(7_500)..=Duration::from_millis(7_504))
                .contains(&started.elapsed()),
            "{request:?} waited {:?}",
            started.elapsed()
        );
        tokio::time::resume();
    }
}

#[test]
fn connection_retry_delay_backs_off_and_is_bounded() {
    let initial = ResponsesStreamRetryState::default().connection_retry_delay;
    assert_eq!(initial, Duration::from_millis(500));

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
async fn connection_recovery_tries_https_before_waiting_without_spending_provider_retries() {
    for max_retries in [0, 2] {
        let home = tempfile::tempdir().unwrap();
        let (session, mut turn, events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test key"),
                Vec::new(),
                home.path(),
                |config| config.model_provider.supports_websockets = true,
            )
            .await;
        std::sync::Arc::get_mut(&mut turn).unwrap().session_source = SessionSource::Cli;
        let mut client = session.services.model_client.new_session();
        let mut state = ResponsesStreamRetryState::default();
        let cancellation = CancellationToken::new();
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        handle_retryable_response_stream_error(
            &mut state,
            max_retries,
            connection_failed(),
            &mut client,
            &session,
            &turn,
            ResponsesStreamRequest::Sampling,
            &cancellation,
        )
        .await
        .unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(!session.services.model_client.responses_websocket_enabled());
        assert_eq!(state.retries, 0);
        assert_eq!(state.connection_retries, 0);
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(emitted.len(), 1);
        assert!(matches!(&emitted[0].msg, EventMsg::Warning(warning)
            if warning.message.contains("Falling back from WebSockets to HTTPS")));

        // Persistent outages still back off on HTTP, without spending provider retries.
        let started = tokio::time::Instant::now();
        handle_retryable_response_stream_error(
            &mut state,
            max_retries,
            connection_failed(),
            &mut client,
            &session,
            &turn,
            ResponsesStreamRequest::Sampling,
            &cancellation,
        )
        .await
        .unwrap();
        assert!(
            (Duration::from_millis(500)..=Duration::from_millis(501)).contains(&started.elapsed())
        );
        assert_eq!(state.retries, 0);
        assert_eq!(state.connection_retries, 1);

        // Once HTTP can dispatch, provider errors retain the original budget,
        // rather than granting a fresh retry window after a transport change.
        for attempt in 0..=max_retries {
            let result = handle_retryable_response_stream_error(
                &mut state,
                max_retries,
                CodexErr::RequestTimeout,
                &mut client,
                &session,
                &turn,
                ResponsesStreamRequest::Sampling,
                &cancellation,
            )
            .await;
            assert_eq!(result.is_ok(), attempt < max_retries);
        }
        assert_eq!(state.retries, max_retries);
        tokio::time::resume();
    }
}

#[test]
fn batch_and_subagent_sources_do_not_opt_into_unlimited_recovery() {
    for source in [
        SessionSource::Exec,
        SessionSource::Unknown,
        SessionSource::Custom("batch-worker".into()),
        SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::Review),
    ] {
        assert!(!should_wait_for_connection_recovery(
            &connection_failed(),
            &source,
            &ModelProviderInfo::default()
        ));
    }
}

#[test]
fn audit_declared_incompletion_and_unknown_failures_do_not_retry_or_switch_transport() {
    for reason in ["max_output_tokens", "content_filter", "future_reason"] {
        let err =
            CodexErr::IncompleteResponse(Box::new(codex_protocol::error::IncompleteResponse {
                response_id: Some("partial".into()),
                reason: reason.into(),
                token_usage: None,
            }));
        assert!(!should_retry_response_stream(&err));
        assert!(!should_switch_fallback_transport(&err));
    }
    let unknown = CodexErr::ProviderFailure {
        code: Some("unknown".into()),
        message: "failure".into(),
    };
    assert!(!should_retry_response_stream(&unknown));
    assert!(!should_switch_fallback_transport(&unknown));
}

#[tokio::test(start_paused = true)]
async fn provider_retry_preserves_jitter_and_prioritizes_cancellation() {
    let error = CodexErr::Stream("retry".into(), None);
    for retry in [1, 5, 6, 100] {
        let mut delays = (0..32)
            .map(|_| response_stream_retry_delay(&error, retry))
            .collect::<Vec<_>>();
        delays.sort_unstable();
        let minimum = delays[0];
        let maximum = delays[delays.len() - 1];
        delays.dedup();
        assert!(maximum <= MAX_RESPONSE_STREAM_RETRY_DELAY);
        if retry == 100 {
            assert!(minimum >= MAX_RESPONSE_STREAM_RETRY_DELAY.mul_f64(0.9));
            assert!(delays.len() > 1, "saturated retries must retain jitter");
        }
    }

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let deadline = tokio::time::Instant::now() - Duration::from_secs(1);
    // Exercise both-ready selection repeatedly to catch accidental unbiased selection.
    for _ in 0..32 {
        assert!(
            matches!(
                wait_for_retry_deadline(deadline, &cancellation).await,
                Err(CodexErr::TurnAborted)
            ),
            "cancellation must win expired deadlines"
        );
    }
}

#[tokio::test]
async fn owner_cancellation_interrupts_each_active_retry_wait() {
    for kind in ["provider", "network", "fallback"] {
        let home = tempfile::tempdir().unwrap();
        let (session, mut turn, events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test key"),
                Vec::new(),
                home.path(),
                |config| config.model_provider.supports_websockets = true,
            )
            .await;
        if kind == "network" {
            std::sync::Arc::get_mut(&mut turn).unwrap().session_source = SessionSource::Cli;
        }
        let mut client = session.services.model_client.new_session();
        if kind == "network" {
            // A failed WebSocket dial now switches to HTTPS immediately. The
            // active offline wait belongs to a subsequent HTTP failure.
            assert!(client.try_switch_fallback_transport(&turn.session_telemetry));
        }
        let mut state = ResponsesStreamRetryState::default();
        let cancellation = CancellationToken::new();
        tokio::time::pause();
        let advice = RetryAfter::from_delay(Duration::from_secs(60));
        let error = match kind {
            "network" => connection_failed(),
            "fallback" => {
                unexpected_status(StatusCode::SERVICE_UNAVAILABLE).with_retry_after(advice)
            }
            _ => CodexErr::Stream("retry later".into(), advice),
        };
        let retry = handle_retryable_response_stream_error(
            &mut state,
            if kind == "fallback" { 0 } else { 5 },
            error,
            &mut client,
            &session,
            &turn,
            ResponsesStreamRequest::Sampling,
            &cancellation,
        );
        tokio::pin!(retry);
        assert!(futures::poll!(retry.as_mut()).is_pending(), "{kind}");
        assert!(
            events.try_recv().is_ok(),
            "{kind} must enter the announced wait"
        );
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_millis(1), retry.as_mut())
            .await
            .expect("cancellation must not wait for the retry deadline");
        assert!(matches!(result, Err(CodexErr::TurnAborted)), "{kind}");
        assert!(
            events.try_recv().is_err(),
            "cancellation must not announce another retry"
        );
        tokio::time::resume();
    }
}

#[tokio::test]
async fn cancelled_retry_does_not_switch_transport_or_announce_an_attempt() {
    let home = tempfile::tempdir().unwrap();
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |config| config.model_provider.supports_websockets = true,
        )
        .await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut client_session = session.services.model_client.new_session();
    let mut retry_state = ResponsesStreamRetryState::default();
    for request in [
        ResponsesStreamRequest::Sampling,
        ResponsesStreamRequest::LocalCompaction,
        ResponsesStreamRequest::RemoteCompactionV2,
    ] {
        for max_retries in [0, 2] {
            let result = handle_retryable_response_stream_error(
                &mut retry_state,
                max_retries,
                codex_api::map_api_error(codex_api::ApiError::Stream("closed".into())),
                &mut client_session,
                &session,
                &turn_context,
                request,
                &cancellation,
            )
            .await;
            assert!(matches!(result, Err(CodexErr::TurnAborted)));
            assert!(session.services.model_client.responses_websocket_enabled());
            assert_eq!(retry_state.retries, 0);
            assert_eq!(retry_state.connection_retries, 0);
            assert!(events.try_recv().is_err());
        }
    }
}

fn store_test_login(codex_home: &std::path::Path, api_key: &str) {
    codex_login::login_with_api_key(
        codex_home,
        api_key,
        codex_login::AuthCredentialsStoreMode::File,
        codex_login::AuthKeyringBackendKind::default(),
    )
    .expect("store test credentials");
}

/// Makes the turn interactive and backed by a first-party account whose stored
/// credentials match the cached ones, as after a normal login.
fn interactive_turn_with_account(
    turn: &mut std::sync::Arc<TurnContext>,
    codex_home: &std::path::Path,
) -> std::sync::Arc<codex_login::AuthManager> {
    store_test_login(codex_home, "first-account");
    let auth_manager = codex_login::AuthManager::from_auth_for_testing_with_home(
        codex_login::CodexAuth::from_api_key("first-account"),
        codex_home.to_path_buf(),
    );
    let turn = std::sync::Arc::get_mut(turn).expect("test turn context should be uniquely owned");
    turn.provider = codex_model_provider::create_model_provider(
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        Some(std::sync::Arc::clone(&auth_manager)),
    );
    turn.session_source = SessionSource::Cli;
    auth_manager
}

fn usage_limit(resets_at: Option<chrono::DateTime<chrono::Utc>>) -> UsageLimitReachedError {
    UsageLimitReachedError {
        plan_type: None,
        resets_at,
        rate_limits: None,
        promo_message: None,
        rate_limit_reached_type: None,
    }
}

#[tokio::test]
async fn missing_credentials_wait_ends_when_a_login_lands() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    let codex_home = tempfile::tempdir().unwrap();
    let auth_manager = interactive_turn_with_account(&mut turn, codex_home.path());
    assert!(!turn.provider.missing_required_credentials());
    auth_manager.logout().await.expect("logout");
    assert!(
        turn.provider.missing_required_credentials(),
        "a logged-out first-party provider would send no credentials"
    );

    let login = tokio::spawn({
        let auth_manager = std::sync::Arc::clone(&auth_manager);
        let codex_home = codex_home.path().to_path_buf();
        async move {
            store_test_login(&codex_home, "second-account");
            auth_manager.reload().await
        }
    });
    let mut deadline = None;
    let retry = tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_account_change(
            AccountChangeWait::MissingCredentials,
            &mut deadline,
            &session,
            &turn,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("a login must end the wait")
    .expect("the wait was not cancelled");
    assert!(retry, "the request is sent once credentials are back");
    assert!(login.await.unwrap(), "the login changed the cached auth");
    assert!(!turn.provider.missing_required_credentials());
    assert!(
        deadline.is_some(),
        "the first wait sets the request's bound"
    );
    let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        emitted.iter().any(|event| matches!(&event.msg,
        EventMsg::Warning(warning) if warning.message.contains("No model credentials are loaded")))
    );
}

#[tokio::test]
async fn usage_limit_wait_ends_at_a_reset_inside_its_bound() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    let codex_home = tempfile::tempdir().unwrap();
    let _auth_manager = interactive_turn_with_account(&mut turn, codex_home.path());
    tokio::time::pause();
    let error = usage_limit(Some(chrono::Utc::now() + chrono::Duration::seconds(90)));
    let started = tokio::time::Instant::now();
    let mut deadline = None;
    let retry = wait_for_account_change(
        AccountChangeWait::UsageLimit(&error, None),
        &mut deadline,
        &session,
        &turn,
        &CancellationToken::new(),
    )
    .await
    .expect("the wait was not cancelled");
    let waited = started.elapsed();
    tokio::time::resume();
    assert!(retry, "a reset inside the bound retries the request");
    // The reset, not the 15-minute bound, ends the wait. Credential reloads
    // while waiting find the same account and do not wake it early.
    assert!(
        waited >= Duration::from_secs(89) && waited < ACCOUNT_CHANGE_WAIT,
        "waited {waited:?}"
    );
    let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(emitted.iter().any(|event| matches!(&event.msg,
        EventMsg::Warning(warning)
            if warning.message.contains("Waiting up to 15 min for an account switch or the limit reset"))));
}

#[tokio::test]
async fn owner_cancellation_interrupts_the_account_wait() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    let codex_home = tempfile::tempdir().unwrap();
    let _auth_manager = interactive_turn_with_account(&mut turn, codex_home.path());
    let error = usage_limit(/*resets_at*/ None);
    let cancellation = CancellationToken::new();
    let mut deadline = None;
    tokio::time::pause();
    let wait = wait_for_account_change(
        AccountChangeWait::UsageLimit(&error, None),
        &mut deadline,
        &session,
        &turn,
        &cancellation,
    );
    tokio::pin!(wait);
    assert!(futures::poll!(wait.as_mut()).is_pending());
    assert!(events.try_recv().is_ok(), "the wait is announced");
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_millis(1), wait.as_mut())
        .await
        .expect("cancellation must not wait for the account bound");
    tokio::time::resume();
    assert!(matches!(result, Err(CodexErr::TurnAborted)));
}

#[tokio::test]
async fn account_wait_keeps_batch_sessions_and_spent_bounds_failing_fast() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    let codex_home = tempfile::tempdir().unwrap();
    let _auth_manager = interactive_turn_with_account(&mut turn, codex_home.path());
    let error = usage_limit(/*resets_at*/ None);

    // An exhausted bound keeps the original error without another announcement.
    let mut spent = Some(tokio::time::Instant::now());
    assert!(
        !wait_for_account_change(
            AccountChangeWait::UsageLimit(&error, None),
            &mut spent,
            &session,
            &turn,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    );

    // Batch and internal sessions fail fast for their callers.
    for source in [
        SessionSource::Exec,
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation),
        SessionSource::SubAgent(SubAgentSource::Review),
    ] {
        std::sync::Arc::get_mut(&mut turn).unwrap().session_source = source.clone();
        let mut deadline = None;
        assert!(
            !wait_for_account_change(
                AccountChangeWait::UsageLimit(&error, None),
                &mut deadline,
                &session,
                &turn,
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
            "{source:?}"
        );
        assert!(deadline.is_none(), "{source:?}");
    }
    assert!(events.try_recv().is_err(), "no wait was announced");

    // Spawned agents wait with their interactive parent.
    assert!(waits_for_account_change(&SessionSource::SubAgent(
        SubAgentSource::ThreadSpawn {
            parent_thread_id: codex_protocol::ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }
    )));
}
#[tokio::test]
async fn missing_credentials_wait_rechecks_a_login_that_already_completed() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    let codex_home = tempfile::tempdir().unwrap();
    let auth_manager = interactive_turn_with_account(&mut turn, codex_home.path());
    auth_manager.logout().await.unwrap();
    // Model the caller's missing-credentials observation, followed by a login
    // on another task before the wait can subscribe to auth changes.
    assert!(turn.provider.missing_required_credentials());
    store_test_login(codex_home.path(), "second-account");
    assert!(auth_manager.reload().await);
    assert!(!turn.provider.missing_required_credentials());

    tokio::time::pause();
    for cancelled in [false, true] {
        let cancellation = CancellationToken::new();
        if cancelled {
            cancellation.cancel();
        }
        let mut deadline = None;
        let result = tokio::time::timeout(
            Duration::from_millis(1),
            wait_for_account_change(
                AccountChangeWait::MissingCredentials,
                &mut deadline,
                &session,
                &turn,
                &cancellation,
            ),
        )
        .await
        .expect("an already completed login must not wait for another auth change");
        if cancelled {
            assert!(matches!(result, Err(CodexErr::TurnAborted)));
        } else {
            assert!(
                result.unwrap(),
                "the authenticated request can now be retried"
            );
        }
        assert!(deadline.is_none(), "no account wait was needed");
        assert!(events.try_recv().is_err(), "do not announce a stale wait");
    }
    tokio::time::resume();
}

#[tokio::test]
async fn usage_limit_rechecks_the_failed_attempt_account_before_waiting() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn, _) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    let credentials = tempfile::tempdir().unwrap();
    let manager = interactive_turn_with_account(&mut turn, credentials.path());
    let failed_account = AccountChangeIdentity::from_auth(&manager.auth_cached().unwrap()).unwrap();
    store_test_login(credentials.path(), "replacement-account");
    manager.reload().await;
    assert_eq!(
        manager.auth_cached().unwrap().api_key(),
        Some("replacement-account")
    );
    let error = usage_limit(None);
    let mut deadline = None;
    let cancellation = CancellationToken::new();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_account_change(
                AccountChangeWait::UsageLimit(&error, Some(&failed_account)),
                &mut deadline,
                &session,
                &turn,
                &cancellation,
            )
        )
        .await
        .expect("already completed switch must not wait")
        .unwrap()
    );
    // Repeated errors under the replacement cannot reuse the old switch.
    let replacement = AccountChangeIdentity::from_auth(&manager.auth_cached().unwrap()).unwrap();
    let wait = wait_for_account_change(
        AccountChangeWait::UsageLimit(&error, Some(&replacement)),
        &mut deadline,
        &session,
        &turn,
        &cancellation,
    );
    tokio::pin!(wait);
    assert!(futures::poll!(wait.as_mut()).is_pending());
    manager.reload().await;
    assert!(
        futures::poll!(wait.as_mut()).is_pending(),
        "unchanged credentials must not replay"
    );
    cancellation.cancel();
    assert!(matches!(wait.await, Err(CodexErr::TurnAborted)));
}

#[tokio::test]
async fn due_usage_reset_retries_once_then_backs_off_with_the_same_deadline() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn, _) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    let credentials = tempfile::tempdir().unwrap();
    let _manager = interactive_turn_with_account(&mut turn, credentials.path());
    let error = usage_limit(Some(chrono::Utc::now() - chrono::Duration::seconds(1)));
    let mut deadline = None;
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let cancellation = CancellationToken::new();
    assert!(
        wait_for_account_change(
            AccountChangeWait::UsageLimit(&error, None),
            &mut deadline,
            &session,
            &turn,
            &cancellation
        )
        .await
        .unwrap()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let original_deadline = deadline;
    assert!(
        wait_for_account_change(
            AccountChangeWait::UsageLimit(&error, None),
            &mut deadline,
            &session,
            &turn,
            &cancellation
        )
        .await
        .unwrap()
    );
    assert!(started.elapsed() >= ACCOUNT_CHANGE_RELOAD_INTERVAL);
    assert_eq!(deadline, original_deadline);
    cancellation.cancel();
    assert!(matches!(
        wait_for_account_change(
            AccountChangeWait::UsageLimit(&error, None),
            &mut deadline,
            &session,
            &turn,
            &cancellation
        )
        .await,
        Err(CodexErr::TurnAborted)
    ));
    tokio::time::resume();
}

struct Audit143ExternalAuth {
    key: std::sync::Mutex<String>,
    held: std::sync::atomic::AtomicBool,
    calls: std::sync::atomic::AtomicUsize,
}

impl codex_login::ExternalAuth for Audit143ExternalAuth {
    fn resolve(&self) -> codex_login::ExternalAuthFuture<'_, codex_login::CodexAuth> {
        Box::pin(async move {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.held.load(std::sync::atomic::Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(codex_login::CodexAuth::from_api_key(&self.key.lock().unwrap()))
        })
    }

    fn refresh(&self, _: codex_login::ExternalAuthRefreshContext) -> codex_login::ExternalAuthFuture<'_, codex_login::CodexAuth> {
        self.resolve()
    }
}

#[tokio::test]
async fn audit143_external_source_rotation_and_stalled_reload_respect_owner_bounds() {
    for mode in [0, 1, 2] {
        let home = tempfile::tempdir().unwrap();
        let (session, mut turn, _events) = crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test"), Vec::new(), home.path(), |_| {},
        ).await;
        let manager = interactive_turn_with_account(&mut turn, home.path());
        let source = std::sync::Arc::new(Audit143ExternalAuth {
            key: std::sync::Mutex::new("first-account".into()),
            held: std::sync::atomic::AtomicBool::new(false),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        manager.set_configured_external_auth(source.clone()).await.unwrap();
        if mode == 0 { *source.key.lock().unwrap() = "second-account".into(); }
        else { source.held.store(true, std::sync::atomic::Ordering::SeqCst); }
        let failed = AccountChangeIdentity::ApiKey("first-account".into());
        let error = usage_limit(None);
        let cancellation = CancellationToken::new();
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let mut deadline = Some(start + Duration::from_secs(15));
        let wait = wait_for_account_change(AccountChangeWait::UsageLimit(&error, Some(&failed)),
            &mut deadline, &session, &turn, &cancellation);
        tokio::pin!(wait);
        assert!(futures::poll!(wait.as_mut()).is_pending());
        tokio::time::advance(ACCOUNT_CHANGE_RELOAD_INTERVAL).await;
        if mode == 0 {
            assert!(wait.await.unwrap());
        } else {
            assert!(futures::poll!(wait.as_mut()).is_pending());
            if mode == 1 {
                cancellation.cancel();
                assert!(matches!(wait.await, Err(CodexErr::TurnAborted)));
            } else {
                tokio::time::advance(Duration::from_secs(5)).await;
                assert!(!wait.await.unwrap());
            }
        }
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(start.elapsed() <= Duration::from_secs(15));
        tokio::time::resume();
    }
}

#[tokio::test]
async fn audit143_explicit_websocket_rejection_has_one_zero_budget_repair() {
    for reason in [codex_protocol::error::WebsocketRecoveryReason::PreviousResponseNotFound,
        codex_protocol::error::WebsocketRecoveryReason::ConnectionLimitReached] {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let mut client = session.services.model_client.new_session();
        let mut retry = ResponsesStreamRetryState::default();
        let rejected = || CodexErr::WebsocketRejected { reason, message: "rejected".into(), retry_after: None };
        handle_retryable_response_stream_error(&mut retry, 0, rejected(), &mut client, &session,
            &turn, ResponsesStreamRequest::Sampling, &CancellationToken::new()).await.unwrap();
        assert!(handle_retryable_response_stream_error(&mut retry, 0, rejected(), &mut client, &session,
            &turn, ResponsesStreamRequest::Sampling, &CancellationToken::new()).await.is_err());
        assert_eq!(retry.retries, 0);
    }
}
