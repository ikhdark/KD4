use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::time::Instant;

use super::WebSocketPongWatchdog;

#[tokio::test(start_paused = true)]
async fn pong_racing_flush_does_not_leave_a_false_deadline() {
    let liveness = super::WebSocketLiveness::new(Duration::from_secs(2));
    let generation = liveness.pong_generation();
    liveness.received_pong();
    liveness.ping_flushed(generation);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), liveness.expired())
            .await
            .is_err()
    );
    let generation = liveness.pong_generation();
    liveness.ping_flushed(generation);
    tokio::time::timeout(Duration::from_secs(3), liveness.expired())
        .await
        .unwrap();
}

#[test]
fn repeated_ping_does_not_extend_deadline() {
    let started_at = Instant::now();
    let timeout = Duration::from_secs(2);
    let mut watchdog = WebSocketPongWatchdog::new(timeout);

    assert_eq!(watchdog.deadline(), None);
    assert_eq!(watchdog.write_deadline(started_at), started_at + timeout);
    watchdog.ping_sent(started_at);
    watchdog.ping_sent(started_at + Duration::from_secs(1));
    assert_eq!(
        watchdog.write_deadline(started_at + Duration::from_secs(1)),
        started_at + timeout
    );
    assert_eq!(watchdog.deadline(), Some(started_at + timeout));
}

#[test]
fn pong_starts_a_fresh_deadline() {
    let started_at = Instant::now();
    let timeout = Duration::from_secs(2);
    let mut watchdog = WebSocketPongWatchdog::new(timeout);

    watchdog.ping_sent(started_at);
    watchdog.received_pong();
    assert_eq!(watchdog.deadline(), None);
    assert_eq!(
        watchdog.write_deadline(started_at + timeout),
        started_at + timeout + timeout
    );
    watchdog.ping_sent(started_at + timeout);
    assert_eq!(watchdog.deadline(), Some(started_at + timeout + timeout));
}
