use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn blocked_lifecycle_for_one_thread_does_not_delay_another_thread() {
    let coordinator = Arc::new(ThreadLifecycleCoordinator::default());
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let thread_a_guard = coordinator.lock_thread(thread_a).await;

    let coordinator_for_waiter = Arc::clone(&coordinator);
    let mut thread_a_waiter = tokio::spawn(async move {
        let _guard = coordinator_for_waiter.lock_thread(thread_a).await;
    });
    tokio::task::yield_now().await;

    let thread_b_guard = tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.lock_thread(thread_b),
    )
    .await
    .expect("independent thread lifecycle should not wait for thread A");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut thread_a_waiter)
            .await
            .is_err(),
        "same-thread lifecycle work must remain serialized"
    );

    drop(thread_b_guard);
    drop(thread_a_guard);
    tokio::time::timeout(Duration::from_secs(1), thread_a_waiter)
        .await
        .expect("thread A waiter should finish after its keyed lock is released")
        .expect("thread A waiter task should not panic");
}

#[tokio::test]
async fn reversed_subtree_requests_serialize_without_deadlock() {
    let coordinator = Arc::new(ThreadLifecycleCoordinator::default());
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let active = Arc::new(AtomicUsize::new(0));

    let run = |thread_ids: [ThreadId; 2]| {
        let coordinator = Arc::clone(&coordinator);
        let active = Arc::clone(&active);
        async move {
            let _guards = coordinator.lock_threads(thread_ids).await;
            assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
            tokio::task::yield_now().await;
            assert_eq!(active.fetch_sub(1, Ordering::SeqCst), 1);
        }
    };

    tokio::time::timeout(
        Duration::from_secs(1),
        futures::future::join(run([thread_a, thread_b]), run([thread_b, thread_a])),
    )
    .await
    .expect("sorted subtree lock acquisition should not deadlock");
}

#[tokio::test]
async fn blocked_subscription_coordination_is_per_thread() {
    let coordinator = Arc::new(ThreadLifecycleCoordinator::default());
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let mut thread_a_guard = coordinator.subscription_guard(thread_a).await;

    let coordinator_for_waiter = Arc::clone(&coordinator);
    let mut thread_a_waiter = tokio::spawn(async move {
        let _guard = coordinator_for_waiter.subscription_guard(thread_a).await;
    });
    tokio::task::yield_now().await;

    let thread_b_guard = tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.subscription_guard(thread_b),
    )
    .await
    .expect("thread B subscription should not wait for thread A");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut thread_a_waiter)
            .await
            .is_err(),
        "same-thread subscriptions must remain serialized"
    );

    let unload_token = thread_a_guard.mark_unloading();
    drop(thread_a_guard);
    assert!(coordinator.is_unloading(thread_a).await);
    assert!(!thread_b_guard.is_unloading());
    unload_token.clear().await;

    drop(thread_b_guard);
    tokio::time::timeout(Duration::from_secs(1), thread_a_waiter)
        .await
        .expect("thread A subscription should continue after its guard is released")
        .expect("thread A subscription task should not panic");
}
