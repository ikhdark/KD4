use codex_client::ByteStream;
use codex_client::Request;
use codex_client::RetryOn;
use codex_client::RetryPolicy;
use codex_client::TransportError;
use codex_client::backoff;
use codex_client::capped_backoff;
use codex_client::run_with_retry;
use codex_client::run_with_retry_non_idempotent;
use codex_client::sse_stream;
use codex_http_client::RetryAfter;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test(start_paused = true)]
async fn retry_after_deadline_is_not_restarted_and_does_not_allow_unsafe_replay() {
    for (elapsed, expected_wait) in [(4, 6), (12, 0)] {
        let advice = RetryAfter::from_delay(Duration::from_secs(10)).unwrap();
        tokio::time::advance(Duration::from_secs(elapsed)).await;
        let started = tokio::time::Instant::now();
        let mut policy = retry_policy(1);
        policy.base_delay = Duration::from_secs(30);
        let result = run_with_retry(policy, request, |_request, attempt| async move {
            if attempt == 0 {
                Err(TransportError::Http {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    url: None,
                    headers: None,
                    body: None,
                    retry_after: Some(advice),
                })
            } else {
                Ok("recovered")
            }
        })
        .await;
        assert_eq!(result.unwrap(), "recovered");
        assert_eq!(started.elapsed(), Duration::from_secs(expected_wait));
    }
    let advice = RetryAfter::from_delay(Duration::from_secs(10)).unwrap();
    let started = tokio::time::Instant::now();
    let attempts = AtomicU64::new(0);
    let error = run_with_retry_non_idempotent(retry_policy(2), request, |_request, _attempt| {
        attempts.fetch_add(1, Ordering::Relaxed);
        async {
            Err::<(), _>(TransportError::Http {
                status: StatusCode::SERVICE_UNAVAILABLE,
                url: None,
                headers: None,
                body: None,
                retry_after: Some(advice),
            })
        }
    })
    .await
    .unwrap_err();
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(error.retry_after(), Some(advice));
}

fn request() -> Request {
    Request::new(Method::GET, "https://example.test".to_string())
}

fn retry_policy(max_retries: u64) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay: Duration::ZERO,
        retry_on: RetryOn {
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        },
    }
}

#[test]
fn backoff_saturates_large_public_inputs() {
    let large_retry_number = u64::from(u32::MAX) + 1;
    let retry_delay = backoff(Duration::from_millis(1), large_retry_number);
    assert!(retry_delay > Duration::from_secs(60));

    let oversized_base = Duration::from_secs(u64::MAX / 1_000 + 1);
    let base_delay = backoff(oversized_base, 1);
    assert!(base_delay > Duration::from_secs(60));
}

#[test]
fn capped_backoff_preserves_growth_below_the_ceiling() {
    let base = Duration::from_millis(100);
    let maximum = Duration::from_secs(2);
    assert_eq!(capped_backoff(base, 0, maximum), base);
    for (retry, minimum, upper) in [(1, 90, 110), (2, 180, 220)] {
        let delay = capped_backoff(base, retry, maximum);
        assert!((Duration::from_millis(minimum)..Duration::from_millis(upper)).contains(&delay));
    }
    for retry in [100, u64::MAX] {
        let delay = capped_backoff(base, retry, maximum);
        assert!((Duration::from_millis(1_800)..maximum).contains(&delay));
    }
    assert_eq!(capped_backoff(base, 1, Duration::ZERO), Duration::ZERO);
    assert_eq!(capped_backoff(Duration::ZERO, 100, maximum), Duration::ZERO);
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_retry_recovers_pre_dispatch_failures_and_honors_policy() {
    for (enabled, max_retries, expected_attempts) in [(true, 2, 2), (false, 2, 1), (true, 0, 1)] {
        let mut policy = retry_policy(max_retries);
        policy.retry_on.retry_transport = enabled;
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_op = attempts.clone();
        let result = run_with_retry(policy, request, move |_request, attempt| {
            let attempts = attempts_for_op.clone();
            async move {
                assert_eq!(attempts.fetch_add(1, Ordering::Relaxed), attempt);
                if attempt == 0 {
                    Err(TransportError::PreDispatch(
                        "temporary auth failure".to_string(),
                    ))
                } else {
                    Ok("recovered")
                }
            }
        })
        .await;
        if expected_attempts == 2 {
            assert_eq!(result.unwrap(), "recovered");
        } else {
            assert!(
                matches!(result, Err(TransportError::PreDispatch(message)) if message == "temporary auth failure")
            );
        }
        assert_eq!(attempts.load(Ordering::Relaxed), expected_attempts);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn connection_retry_honors_policy_and_non_idempotent_replay_boundary() {
    use codex_http_client::HttpTransport;

    for (enabled, max_retries, non_idempotent, expected_attempts) in [
        (true, 1, false, 2),
        (false, 1, false, 1),
        (true, 0, false, 1),
        (true, 1, true, 1),
    ] {
        let unavailable_server = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = unavailable_server.local_addr().unwrap();
        drop(unavailable_server);
        let transport = codex_http_client::ReqwestTransport::from_http_client(
            codex_http_client::HttpClientBuilder::new()
                // Windows can delay a refused connection beyond the request deadline.
                // Bound the connector first so the transport reports a connection error.
                .connect_timeout(Duration::from_millis(100))
                .build_direct()
                .unwrap(),
        );
        let mut initial_request = Request::new(Method::GET, format!("http://{address}/"));
        initial_request.timeout = Some(Duration::from_secs(2));
        let error = transport.execute(initial_request).await.unwrap_err();
        assert!(
            matches!(error, TransportError::Connection(_)),
            "expected a connection error, got {error:?}"
        );

        let pending_error = std::sync::Mutex::new(Some(error));
        let attempts = AtomicU64::new(0);
        let op = |_request, attempt| {
            assert_eq!(attempts.fetch_add(1, Ordering::Relaxed), attempt);
            let error = pending_error.lock().unwrap().take();
            async move { error.map_or(Ok("recovered"), Err) }
        };
        let mut policy = retry_policy(max_retries);
        policy.retry_on.retry_transport = enabled;
        let result = if non_idempotent {
            run_with_retry_non_idempotent(policy, request, op).await
        } else {
            run_with_retry(policy, request, op).await
        };
        assert_eq!(attempts.load(Ordering::Relaxed), expected_attempts);
        if expected_attempts == 2 {
            assert_eq!(result.unwrap(), "recovered");
        } else {
            assert!(matches!(result, Err(TransportError::Connection(_))));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn zero_retries_makes_one_request() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_op = attempts.clone();

    let result = run_with_retry(retry_policy(0), request, move |_request, attempt| {
        let attempts = attempts_for_op.clone();
        async move {
            attempts.fetch_add(1, Ordering::Relaxed);
            assert_eq!(attempt, 0);
            Err::<(), _>(TransportError::Network("network unavailable".to_string()))
        }
    })
    .await;

    assert!(
        matches!(result, Err(TransportError::Network(message)) if message == "network unavailable")
    );
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn four_retries_make_at_most_five_requests() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_op = attempts.clone();

    let result = run_with_retry(retry_policy(4), request, move |_request, _attempt| {
        let attempts = attempts_for_op.clone();
        async move {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(TransportError::Network("still unavailable".to_string()))
        }
    })
    .await;

    assert!(
        matches!(result, Err(TransportError::Network(message)) if message == "still unavailable")
    );
    assert_eq!(attempts.load(Ordering::Relaxed), 5);
}

#[tokio::test(flavor = "current_thread")]
async fn non_retryable_error_returns_immediately() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_op = attempts.clone();

    let result = run_with_retry(retry_policy(4), request, move |_request, _attempt| {
        let attempts = attempts_for_op.clone();
        async move {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(TransportError::Build("invalid request".to_string()))
        }
    })
    .await;

    assert!(matches!(result, Err(TransportError::Build(message)) if message == "invalid request"));
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn final_underlying_error_is_preserved() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_op = attempts.clone();

    let result = run_with_retry(retry_policy(2), request, move |_request, _attempt| {
        let attempts = attempts_for_op.clone();
        async move {
            attempts.fetch_add(1, Ordering::Relaxed);
            let mut headers = HeaderMap::new();
            headers.insert("x-request-id", HeaderValue::from_static("request-123"));
            Err::<(), _>(TransportError::Http {
                retry_after: None,
                status: StatusCode::INTERNAL_SERVER_ERROR,
                url: Some("https://example.test".to_string()),
                headers: Some(headers),
                body: Some("provider failure".to_string()),
            })
        }
    })
    .await;

    let Err(TransportError::Http {
        retry_after: _,
        status,
        url,
        headers,
        body,
    }) = result
    else {
        panic!("expected the final HTTP error");
    };
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(url.as_deref(), Some("https://example.test"));
    assert_eq!(
        headers
            .as_ref()
            .and_then(|headers| headers.get("x-request-id")),
        Some(&HeaderValue::from_static("request-123"))
    );
    assert_eq!(body.as_deref(), Some("provider failure"));
    assert_eq!(attempts.load(Ordering::Relaxed), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn retry_classifier_retries_only_proven_safe_failures() {
    let pre_dispatch_attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_op = pre_dispatch_attempts.clone();
    let result =
        run_with_retry_non_idempotent(retry_policy(2), request, move |_request, attempt| {
            let attempts = attempts_for_op.clone();
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                if attempt == 0 {
                    Err(TransportError::PreDispatch(
                        "temporary auth lookup failure".to_string(),
                    ))
                } else {
                    Ok(())
                }
            }
        })
        .await;
    assert!(result.is_ok());
    assert_eq!(pre_dispatch_attempts.load(Ordering::Relaxed), 2);

    for unsafe_error in [
        TransportError::Network("ambiguous socket failure".to_string()),
        TransportError::Timeout,
        TransportError::Http {
            retry_after: None,
            status: StatusCode::INTERNAL_SERVER_ERROR,
            url: None,
            headers: None,
            body: None,
        },
    ] {
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_op = attempts.clone();
        let error = Arc::new(std::sync::Mutex::new(Some(unsafe_error)));
        let error_for_op = error.clone();
        let result =
            run_with_retry_non_idempotent(retry_policy(2), request, move |_request, _attempt| {
                let attempts = attempts_for_op.clone();
                let error = error_for_op.clone();
                async move {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    Err::<(), _>(error.lock().expect("error mutex poisoned").take().unwrap())
                }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn clean_sse_eof_closes_the_output_channel() {
    let stream: ByteStream = Box::pin(futures::stream::empty());
    let (tx, mut rx) = mpsc::channel(1);

    sse_stream(stream, Duration::from_secs(1), tx);

    assert!(rx.recv().await.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn sse_forwards_complete_event_data() {
    let stream: ByteStream = Box::pin(futures::stream::iter([Ok(
        b"data: first\ndata: second\n\ndata: last\n\n"
            .to_vec()
            .into(),
    )]));
    let (tx, mut rx) = mpsc::channel(1);
    sse_stream(stream, Duration::from_secs(60), tx);
    assert_eq!(rx.recv().await.unwrap().unwrap(), "first\nsecond");
    assert_eq!(rx.recv().await.unwrap().unwrap(), "last");
    assert!(rx.recv().await.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_sse_receiver_releases_idle_input() {
    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.take().unwrap().send(()).ok();
        }
    }

    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let guard = DropSignal(Some(dropped_tx));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let mut started_tx = Some(started_tx);
    let stream: ByteStream = Box::pin(futures::stream::poll_fn(move |_cx| {
        let _keep_alive = &guard;
        if let Some(tx) = started_tx.take() {
            tx.send(()).unwrap();
        }
        std::task::Poll::Pending
    }));
    let (tx, rx) = mpsc::channel(1);
    sse_stream(stream, Duration::from_secs(60), tx);
    started_rx.await.unwrap();
    drop(rx);
    tokio::time::timeout(Duration::from_secs(1), dropped_rx)
        .await
        .expect("receiver closure must release input before the idle timeout")
        .expect("input must be dropped");
}
