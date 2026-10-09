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
async fn connection_recovery_reaches_https_fallback_without_exhausting_network_waits() {
    let home = tempfile::tempdir().unwrap();
    let (session, mut turn_context, events) =
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
    // Shared fixtures default to batch Exec; this test is interactive.
    std::sync::Arc::get_mut(&mut turn_context)
        .expect("test turn context should be uniquely owned")
        .session_source = SessionSource::Cli;
    tokio::time::pause();

    for expected_delay in [Duration::from_millis(500), Duration::from_secs(1)] {
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
        EventMsg::StreamError(error) if error.message.contains("attempt 1, next retry in 500.0ms"))));
    assert!(emitted.iter().any(|event| matches!(&event.msg,
        EventMsg::StreamError(error) if error.message.contains("attempt 2, next retry in 1.0s"))));

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
    assert!((Duration::from_secs(2)..=Duration::from_millis(2_001)).contains(&started.elapsed()));
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
