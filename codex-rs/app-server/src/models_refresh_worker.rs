use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_http_client::HttpClientFactory;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MODELS_REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60);

#[derive(Debug)]
pub(crate) struct ModelsRefreshWorker {
    shutdown: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ModelsRefreshWorker {
    pub(crate) fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub(crate) async fn shutdown_and_wait(&self) {
        self.shutdown.cancel();
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for ModelsRefreshWorker {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

pub(crate) fn spawn(
    models_manager: &SharedModelsManager,
    http_client_factory: HttpClientFactory,
) -> ModelsRefreshWorker {
    spawn_with_interval(models_manager, http_client_factory, MODELS_REFRESH_INTERVAL)
}

fn spawn_with_interval(
    models_manager: &SharedModelsManager,
    http_client_factory: HttpClientFactory,
    refresh_interval: Duration,
) -> ModelsRefreshWorker {
    let models_manager = Arc::downgrade(models_manager);
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        loop {
            // Model-dependent requests refresh an empty cache on demand. Wait
            // before forcing a refresh so app-server startup does not issue an
            // otherwise unused network request.
            tokio::select! {
                _ = worker_shutdown.cancelled() => break,
                _ = tokio::time::sleep(refresh_interval) => {}
            }
            if worker_shutdown.is_cancelled() {
                break;
            }
            let Some(models_manager) = models_manager.upgrade() else {
                break;
            };
            let refresh =
                models_manager.list_models(RefreshStrategy::Online, http_client_factory.clone());
            tokio::select! {
                _ = worker_shutdown.cancelled() => break,
                result = refresh => {
                    if let Err(error) = result {
                        tracing::warn!(?error, "periodic model catalog refresh failed");
                    }
                }
            }
            drop(models_manager);
        }
    });
    ModelsRefreshWorker {
        shutdown,
        task: Mutex::new(Some(task)),
    }
}

#[cfg(test)]
#[path = "models_refresh_worker_tests.rs"]
mod tests;
