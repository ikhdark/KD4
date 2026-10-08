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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_from_the_initial_delay_and_saturates() {
        // Zero and one select the first interval, with independent 10% jitter.
        for (attempt, low_ms, high_ms) in [
            (0, 180, 220),
            (1, 180, 220),
            (2, 360, 440),
            (3, 720, 880),
            (4, 1_440, 1_760),
        ] {
            let delay = backoff(attempt);
            assert!(
                delay >= Duration::from_millis(low_ms),
                "{attempt}: {delay:?}"
            );
            assert!(
                delay <= Duration::from_millis(high_ms),
                "{attempt}: {delay:?}"
            );
        }
        for attempt in [100, u64::MAX] {
            let delay = backoff(attempt);
            assert!((Duration::from_secs(27)..Duration::from_secs(30)).contains(&delay));
        }
    }
}
