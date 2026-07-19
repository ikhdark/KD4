use codex_client::ByteStream;
use codex_client::Request;
use codex_client::RetryOn;
use codex_client::RetryPolicy;
use codex_client::TransportError;
use codex_client::backoff;
use codex_client::run_with_retry;
use codex_client::sse_stream;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;

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
                status: StatusCode::INTERNAL_SERVER_ERROR,
                url: Some("https://example.test".to_string()),
                headers: Some(headers),
                body: Some("provider failure".to_string()),
            })
        }
    })
    .await;

    let Err(TransportError::Http {
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
async fn clean_sse_eof_closes_the_output_channel() {
    let stream: ByteStream = Box::pin(futures::stream::empty());
    let (tx, mut rx) = mpsc::channel(1);

    sse_stream(stream, Duration::from_secs(1), tx);

    assert!(rx.recv().await.is_none());
}
