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
    fail_first_fetch: bool,
    fetch_count: AtomicUsize,
    fetched: Notify,
    release_fetch: Notify,
}

impl TestModelsEndpoint {
    fn new(fail_first_fetch: bool) -> Arc<Self> {
        Arc::new(Self {
            fail_first_fetch,
            fetch_count: AtomicUsize::new(0),
            fetched: Notify::new(),
            release_fetch: Notify::new(),
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
            if fetch_index == 0 && self.fail_first_fetch {
                return Err(CodexErr::Io(std::io::Error::other("test failure")));
            }
            if fetch_index == usize::from(self.fail_first_fetch) {
                self.release_fetch.notified().await;
            }
            Ok((vec![refreshed_test_model()], None))
        })
    }
}

fn refreshed_test_model() -> ModelInfo {
    let mut model = codex_models_manager::bundled_models_response()
        .expect("bundled catalog")
        .models
        .remove(0);
    model.slug = "fresh-test-model".to_string();
    model
}

#[tokio::test(start_paused = true)]
async fn activity_before_deadline_arms_remaining_delay_and_periodic_refresh() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(/*fail_first_fetch*/ true);
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

    tokio::time::advance(refresh_interval / 2).await;
    models_manager.get_remote_models().await;
    tokio::task::yield_now().await;
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 0);

    tokio::time::advance(refresh_interval / 2).await;
    endpoint.wait_for_fetch_count(/*expected*/ 1).await;

    tokio::time::advance(refresh_interval).await;
    endpoint.wait_for_fetch_count(/*expected*/ 2).await;
    worker.shutdown_and_wait().await;
    assert_eq!(Arc::strong_count(&models_manager), 1);
    tokio::time::advance(refresh_interval * 2).await;
    tokio::task::yield_now().await;

    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn activity_after_deadline_refreshes_before_serving_catalog() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(/*fail_first_fetch*/ false);
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

    tokio::time::advance(refresh_interval * 3).await;
    tokio::task::yield_now().await;
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 0);

    let catalog_read = models_manager.get_remote_models();
    tokio::pin!(catalog_read);
    assert!(futures::poll!(&mut catalog_read).is_pending());
    endpoint.wait_for_fetch_count(1).await;
    assert!(futures::poll!(&mut catalog_read).is_pending());
    endpoint.release_fetch.notify_one();
    assert_eq!(catalog_read.await, vec![refreshed_test_model()]);
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 1);
    worker.shutdown_and_wait().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_and_joins_an_inflight_refresh() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(/*fail_first_fetch*/ true);
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

    models_manager.get_remote_models().await;
    tokio::task::yield_now().await;
    tokio::time::advance(refresh_interval).await;
    endpoint.wait_for_fetch_count(1).await;
    tokio::time::advance(refresh_interval).await;
    endpoint.wait_for_fetch_count(2).await;

    worker.shutdown_and_wait().await;

    assert!(worker.task.lock().expect("worker task lock").is_none());
    tokio::time::timeout(Duration::from_secs(1), models_manager.get_remote_models())
        .await
        .expect("shutdown must release catalog readers");
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn dropping_worker_before_first_poll_releases_catalog_readers() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(/*fail_first_fetch*/ true);
    let models_manager: SharedModelsManager = Arc::new(OpenAiModelsManager::new(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        None,
        Arc::new(|| "test-provider-identity".to_string()),
    ));
    let interval = Duration::from_secs(10);
    let worker = spawn_with_interval(
        &models_manager,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        interval,
    );
    drop(worker);
    tokio::time::advance(interval).await;
    tokio::time::timeout(Duration::from_secs(1), models_manager.get_remote_models())
        .await
        .expect("aborting an unpolled worker must release catalog readers");
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 0);
}
