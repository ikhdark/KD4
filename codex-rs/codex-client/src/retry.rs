use codex_http_client::Request;
use codex_http_client::TransportError;
use rand::Rng;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

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
            TransportError::Timeout | TransportError::Network(_) => self.retry_transport,
            _ => false,
        }
    }
}

/// Computes exponential backoff for a one-based retry number.
///
/// A retry number of zero is accepted for compatibility and returns `base`
/// without jitter. Retry number one is the first retry.
pub fn backoff(base: Duration, retry_number: u64) -> Duration {
    if retry_number == 0 {
        return base;
    }
    let exponent = u32::try_from(retry_number - 1).unwrap_or(u32::MAX);
    let exp = 2u64.saturating_pow(exponent);
    let millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let raw = millis.saturating_mul(exp);
    let jitter: f64 = rand::rng().random_range(0.9..1.1);
    Duration::from_millis((raw as f64 * jitter) as u64)
}

/// Runs an operation once and retries it up to `policy.max_retries` times.
///
/// The operation receives a zero-based attempt index. If all allowed attempts
/// fail, the final underlying error is returned unchanged.
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
                sleep(backoff(policy.base_delay, retry_number)).await;
                attempt = retry_number;
            }
            Err(err) => return Err(err),
        }
    }
}
