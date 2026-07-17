use super::*;
use std::sync::Arc;
use tokio::sync::Notify;

#[tokio::test]
async fn slow_candidate_does_not_hold_publication_lock() {
    let coordinator = Arc::new(McpProjectionCoordinator::new());
    let slow_ticket = coordinator.begin();
    let slow_started = Arc::new(Notify::new());
    let release_slow = Arc::new(Notify::new());

    let slow_task = {
        let coordinator = Arc::clone(&coordinator);
        let slow_started = Arc::clone(&slow_started);
        let release_slow = Arc::clone(&release_slow);
        tokio::spawn(async move {
            slow_started.notify_one();
            release_slow.notified().await;
            coordinator.lock_if_current(slow_ticket).await.is_some()
        })
    };
    slow_started.notified().await;

    let fast_ticket = coordinator.begin();
    let fast_guard = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        coordinator.lock_if_current(fast_ticket),
    )
    .await
    .expect("slow candidate work must not hold the publication lock")
    .expect("newest candidate should publish");
    drop(fast_guard);

    release_slow.notify_one();
    assert!(!slow_task.await.expect("slow task should finish"));
}

#[tokio::test]
async fn stale_candidate_cannot_publish_after_newer_generation() {
    let coordinator = McpProjectionCoordinator::new();
    let stale = coordinator.begin();
    let current = coordinator.begin();

    assert!(coordinator.lock_if_current(stale).await.is_none());
    assert!(coordinator.lock_if_current(current).await.is_some());
}
