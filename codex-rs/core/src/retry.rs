use std::time::Duration;

const INITIAL_DELAY: Duration = Duration::from_millis(200);
const MAX_DELAY: Duration = Duration::from_secs(30);

/// Uses a one-based retry number; zero selects the first interval for compatibility.
pub fn backoff(attempt: u64) -> Duration {
    backoff_with_cap(attempt, MAX_DELAY)
}

/// Apply the caller's ceiling before jitter so saturated clients stay spread out.
pub(crate) fn backoff_with_cap(attempt: u64, maximum: Duration) -> Duration {
    codex_client::capped_backoff(INITIAL_DELAY, attempt.max(1), maximum.min(MAX_DELAY))
}
