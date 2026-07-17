use super::StartupDiscovery;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

#[tokio::test]
async fn startup_discovery_runs_while_configuration_publication_continues_then_joins() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicBool::new(false));
    let discovery = StartupDiscovery::spawn({
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        let completed = Arc::clone(&completed);
        async move {
            started.notify_one();
            release.notified().await;
            completed.store(true, Ordering::Release);
            "catalog-ready"
        }
    });

    started.notified().await;
    let (configured_tx, configured_rx) = tokio::sync::oneshot::channel();
    configured_tx
        .send("session-configured")
        .expect("publish configuration");
    assert_eq!(configured_rx.await.expect("configuration event"), "session-configured");
    assert!(!completed.load(Ordering::Acquire));
    release.notify_one();
    assert_eq!(
        discovery.wait().await.expect("discovery task"),
        "catalog-ready"
    );
}

#[tokio::test]
async fn dropping_startup_discovery_cancels_and_drops_inflight_work() {
    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    let started = Arc::new(Notify::new());
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let discovery = StartupDiscovery::spawn({
        let started = Arc::clone(&started);
        async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            started.notify_one();
            std::future::pending::<()>().await;
        }
    });

    started.notified().await;
    drop(discovery);
    tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
        .await
        .expect("cancelled discovery should be dropped")
        .expect("drop signal sender");
}
