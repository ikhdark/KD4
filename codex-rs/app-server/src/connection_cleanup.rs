use std::future::Future;
use std::future::pending;

use tokio::task::JoinError;
use tokio::task::JoinSet;
use tracing::warn;

pub(crate) struct ConnectionCleanupTasks {
    tasks: JoinSet<()>,
}

impl ConnectionCleanupTasks {
    pub(crate) fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    pub(crate) fn spawn(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(future);
    }

    pub(crate) fn len(&self) -> usize {
        self.tasks.len()
    }

    pub(crate) fn reap_ready(&mut self) {
        // Keep completion reaping bounded per dispatcher iteration, but do not
        // retain an arbitrary churn backlog one result at a time.
        for _ in 0..32 {
            let Some(result) = self.tasks.try_join_next() else { break };
            log_cleanup_result(result);
        }
    }

    pub(crate) async fn reap_next(&mut self) {
        if self.tasks.is_empty() {
            pending::<()>().await;
        }
        if let Some(result) = self.tasks.join_next().await {
            log_cleanup_result(result);
            self.reap_ready();
        }
    }

    pub(crate) async fn drain(&mut self) {
        while let Some(result) = self.tasks.join_next().await {
            log_cleanup_result(result);
        }
    }

    pub(crate) fn abort(&mut self) {
        self.tasks.abort_all();
    }
}

fn log_cleanup_result(result: Result<(), JoinError>) {
    if let Err(err) = result
        && !err.is_cancelled()
    {
        warn!("connection cleanup task failed: {err}");
    }
}
