use std::time::Duration;

const INITIAL_DELAY: Duration = Duration::from_millis(200);
const MAX_DELAY: Duration = Duration::from_secs(30);

pub fn backoff(attempt: u64) -> Duration {
    codex_client::capped_backoff(INITIAL_DELAY, attempt.max(1), MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded() {
        let delay = backoff(100);
        assert!(!delay.is_zero());
        assert!(delay <= MAX_DELAY);
    }
}
