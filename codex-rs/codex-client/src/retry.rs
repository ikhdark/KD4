use codex_http_client::Request;
use codex_http_client::TransportError;
use rand::Rng;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

/// Longest self-imposed wait between attempts. Server `Retry-After` advice is
/// a minimum wait and is not capped.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retries after the initial request.
    pub max_retries: u64,
    pub base_delay: Duration,
    pub retry_on: RetryOn,
}

#[derive(Debug, Clone)]
pub struct RetryOn {
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryOn {
    /// Returns whether an error from the zero-based `attempt` may be retried.
    pub fn should_retry(&self, err: &TransportError, attempt: u64, max_retries: u64) -> bool {
        if attempt >= max_retries {
            return false;
        }
        match err {
            TransportError::Http { status, .. } => {
                (self.retry_429 && status.as_u16() == 429)
                    || (self.retry_5xx && status.is_server_error())
            }
            TransportError::Timeout
            | TransportError::Connection(_)
            | TransportError::Network(_)
            | TransportError::PreDispatch(_) => self.retry_transport,
            _ => false,
        }
    }

    /// Returns whether a non-idempotent request can be replayed without a
    /// risk of duplicating server-side work.
    pub fn should_retry_non_idempotent(
        &self,
        err: &TransportError,
        attempt: u64,
        max_retries: u64,
    ) -> bool {
        attempt < max_retries
            && self.retry_transport
            && matches!(err, TransportError::PreDispatch(_))
    }
}

/// Computes exponential backoff for a one-based retry number while retaining
/// jitter at the `maximum`. Saturated delays are spread over 90-100% of the
/// ceiling. A retry number of zero returns `base` without jitter.
pub fn capped_backoff(base: Duration, retry_number: u64, maximum: Duration) -> Duration {
    if retry_number == 0 {
        return base.min(maximum);
    }
    let raw = Duration::from_millis(exponential_backoff_millis(base, retry_number));
    let bounded = raw.min(maximum);
    let upper = if raw >= maximum { 1.0 } else { 1.1 };
    let jitter: f64 = rand::rng().random_range(0.9..upper);
    bounded.mul_f64(jitter).min(maximum)
}

fn exponential_backoff_millis(base: Duration, retry_number: u64) -> u64 {
    let exponent = u32::try_from(retry_number.saturating_sub(1)).unwrap_or(u32::MAX);
    let exp = 2u64.saturating_pow(exponent);
    let millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    millis.saturating_mul(exp)
}

/// Runs an operation once and retries it up to `policy.max_retries` times.
///
/// The operation receives a zero-based attempt index. If all allowed attempts
/// fail, the final underlying error is returned unchanged.
/// Use only for operations whose replay is safe. Otherwise use
/// [`run_with_retry_non_idempotent`]. Backoff grows exponentially up to
/// 30 seconds per retry; callers that need an elapsed-time budget must apply
/// an outer deadline.
pub async fn run_with_retry<T, F, Fut>(
    policy: RetryPolicy,
    mut make_req: impl FnMut() -> Request,
    op: F,
) -> Result<T, TransportError>
where
    F: Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    let mut attempt = 0;
    loop {
        let req = make_req();
        match op(req, attempt).await {
            Ok(resp) => return Ok(resp),
            Err(err)
                if policy
                    .retry_on
                    .should_retry(&err, attempt, policy.max_retries) =>
            {
                let retry_number = attempt + 1;
                if let Some(advice) = err.retry_after() {
                    tokio::time::sleep_until(advice.deadline()).await;
                } else {
                    sleep(capped_backoff(
                        policy.base_delay,
                        retry_number,
                        MAX_RETRY_DELAY,
                    ))
                    .await;
                }
                attempt = retry_number;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Runs a non-idempotent operation, replaying only failures that are proven to
/// have happened before transport dispatch.
pub async fn run_with_retry_non_idempotent<T, F, Fut>(
    policy: RetryPolicy,
    mut make_req: impl FnMut() -> Request,
    op: F,
) -> Result<T, TransportError>
where
    F: Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    let mut attempt = 0;
    loop {
        let req = make_req();
        match op(req, attempt).await {
            Ok(resp) => return Ok(resp),
            Err(err)
                if policy.retry_on.should_retry_non_idempotent(
                    &err,
                    attempt,
                    policy.max_retries,
                ) =>
            {
                let retry_number = attempt + 1;
                sleep(capped_backoff(
                    policy.base_delay,
                    retry_number,
                    MAX_RETRY_DELAY,
                ))
                .await;
                attempt = retry_number;
            }
            Err(err) => return Err(err),
        }
    }
}
