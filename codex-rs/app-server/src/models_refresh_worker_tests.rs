use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ModelInfo;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio::sync::Notify;

use super::*;

#[derive(Debug)]
struct TestModelsEndpoint {
    fetch_count: AtomicUsize,
    fetched: Notify,
    release_second_fetch: Notify,
}

impl TestModelsEndpoint {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fetch_count: AtomicUsize::new(0),
            fetched: Notify::new(),
            release_second_fetch: Notify::new(),
        })
    }

    async fn wait_for_fetch_count(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while self.fetch_count.load(Ordering::SeqCst) < expected {
                self.fetched.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {expected} model fetches"));
    }
}

impl ModelsEndpointClient for TestModelsEndpoint {
    fn has_command_auth(&self) -> bool {
        true
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn list_models<'a>(
        &'a self,
        _client_version: &'a str,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>> {
        Box::pin(async move {
            let fetch_index = self.fetch_count.fetch_add(1, Ordering::SeqCst);
            self.fetched.notify_one();
            if fetch_index == 0 {
                return Err(CodexErr::Io(std::io::Error::other("test failure")));
            }
            if fetch_index == 1 {
                self.release_second_fetch.notified().await;
            }
            Ok((Vec::new(), None))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn defers_initial_refresh_then_refreshes_periodically_and_stops_when_dropped() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new();
    let models_manager: SharedModelsManager = Arc::new(OpenAiModelsManager::new(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        /*auth_manager*/ None,
        Arc::new(|| "test-provider-identity".to_string()),
    ));
    let refresh_interval = Duration::from_secs(10);
    let worker = spawn_with_interval(
        &models_manager,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        refresh_interval,
    );

    tokio::task::yield_now().await;
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 0);

    tokio::time::advance(refresh_interval).await;
    endpoint.wait_for_fetch_count(/*expected*/ 1).await;

    tokio::time::advance(refresh_interval).await;
    endpoint.wait_for_fetch_count(/*expected*/ 2).await;
    drop(worker);
    endpoint.release_second_fetch.notify_one();
    tokio::time::advance(refresh_interval * 2).await;
    tokio::task::yield_now().await;

    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 2);
}
