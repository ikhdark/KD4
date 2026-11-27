use super::LocalThreadStore;
use std::sync::Arc;
use std::time::Duration;

impl LocalThreadStore {
    /// Last-store drop cancels future passes; a committed deletion finishes.
    ///
    /// Call from a blocking context: startup reads the retention policy synchronously.
    pub fn with_archive_retention(mut self) -> Self {
        if !codex_rollout::home_retention_enabled(&self.config.codex_home) {
            return self;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return self;
        };
        let worker = Self::new(self.config.clone(), self.state_db.clone());
        let (owner, mut stop) = tokio::sync::watch::channel(());
        self.archive_retention = Some(Arc::new(owner));
        runtime.spawn(async move {
            loop {
                if stop.has_changed().is_err() {
                    break;
                }
                if let Err(err) = worker.run_archive_retention().await {
                    tracing::warn!(%err, "home retention incomplete");
                }
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(6 * 60 * 60)) => {}
                }
            }
        });
        self
    }
}
