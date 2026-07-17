use super::resolve_initial_model;
use crate::config::Config;
use crate::config::test_config;
use codex_http_client::HttpClientFactory;
use codex_models_manager::bundled_models_response;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ModelInfo;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Semaphore;

const TEST_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct BlockingModelsEndpoint {
    response: Mutex<Vec<ModelInfo>>,
    started: Semaphore,
    release: Semaphore,
    completed: Semaphore,
}

impl BlockingModelsEndpoint {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            response: Mutex::new(Vec::new()),
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            completed: Semaphore::new(0),
        })
    }

    fn set_response(&self, response: Vec<ModelInfo>) {
        *self.response.lock().expect("response lock should not be poisoned") = response;
    }

    async fn wait_started(&self) {
        tokio::time::timeout(TEST_TIMEOUT, self.started.acquire())
            .await
            .expect("catalog fetch should start")
            .expect("started semaphore should remain open")
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }

    async fn wait_completed(&self) {
        tokio::time::timeout(TEST_TIMEOUT, self.completed.acquire())
            .await
            .expect("catalog fetch should complete")
            .expect("completed semaphore should remain open")
            .forget();
    }
}

impl ModelsEndpointClient for BlockingModelsEndpoint {
    fn has_command_auth(&self) -> bool {
        false
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { true })
    }

    fn list_models<'a>(
        &'a self,
        _client_version: &'a str,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>> {
        Box::pin(async move {
            self.started.add_permits(1);
            self.release
                .acquire()
                .await
                .expect("release semaphore should remain open")
                .forget();
            let response = self
                .response
                .lock()
                .expect("response lock should not be poisoned")
                .clone();
            self.completed.add_permits(1);
            Ok((response, None))
        })
    }
}

struct ModelResolutionHarness {
    _codex_home: TempDir,
    endpoint: Arc<BlockingModelsEndpoint>,
    manager: SharedModelsManager,
    known_model: String,
    model_template: ModelInfo,
}

impl ModelResolutionHarness {
    fn new() -> Self {
        let codex_home = tempfile::tempdir().expect("temporary CODEX_HOME");
        let endpoint = BlockingModelsEndpoint::new();
        let manager: SharedModelsManager = Arc::new(OpenAiModelsManager::new(
            codex_home.path().to_path_buf(),
            endpoint.clone(),
            /*auth_manager*/ None,
        ));
        let known_model = manager
            .try_list_models()
            .expect("bundled model catalog should be available")
            .into_iter()
            .next()
            .expect("bundled model catalog should contain a picker-visible model")
            .model;
        let model_template = bundled_models_response()
            .expect("bundled model catalog should parse")
            .models
            .into_iter()
            .next()
            .expect("bundled model catalog should contain metadata");
        Self {
            _codex_home: codex_home,
            endpoint,
            manager,
            known_model,
            model_template,
        }
    }

    fn remote_model(&self, slug: &str) -> ModelInfo {
        ModelInfo {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            ..self.model_template.clone()
        }
    }

    async fn config_with_model(&self, model: Option<String>) -> Config {
        let mut config = test_config().await;
        config.model = model;
        config
    }
}

async fn assert_resolution_blocks(
    harness: &ModelResolutionHarness,
    config: Config,
    allow_provider_model_fallback: bool,
) -> (String, ModelInfo) {
    let models_manager = Arc::clone(&harness.manager);
    let resolve_task = tokio::spawn(async move {
        resolve_initial_model(
            &config,
            allow_provider_model_fallback,
            &models_manager,
            RefreshStrategy::OnlineIfUncached,
        )
        .await
    });

    harness.endpoint.wait_started().await;
    assert!(
        !resolve_task.is_finished(),
        "initial model resolution should still be waiting for the catalog"
    );
    harness.endpoint.release();
    tokio::time::timeout(TEST_TIMEOUT, resolve_task)
        .await
        .expect("initial model resolution should finish after catalog refresh")
        .expect("initial model resolution task should not panic")
}

#[tokio::test]
async fn known_explicit_root_model_uses_local_metadata_while_catalog_refreshes() {
    let harness = ModelResolutionHarness::new();
    let config = harness
        .config_with_model(Some(harness.known_model.clone()))
        .await;

    let (model, model_info) = tokio::time::timeout(
        TEST_TIMEOUT,
        resolve_initial_model(
            &config,
            /*allow_provider_model_fallback*/ false,
            &harness.manager,
            RefreshStrategy::OnlineIfUncached,
        ),
    )
    .await
    .expect("known explicit model should not wait for the online catalog");

    assert_eq!(model, harness.known_model);
    assert_eq!(model_info.slug, harness.known_model);
    assert!(!model_info.used_fallback_model_metadata);
    harness.endpoint.wait_started().await;
    harness.endpoint.release();
    harness.endpoint.wait_completed().await;
}

#[tokio::test]
async fn unknown_and_provider_mismatched_models_wait_for_catalog_refresh() {
    for provider_mismatched in [false, true] {
        let harness = ModelResolutionHarness::new();
        let requested_model = if provider_mismatched {
            harness.known_model.clone()
        } else {
            "remote-only-model".to_string()
        };
        harness
            .endpoint
            .set_response(vec![harness.remote_model(&requested_model)]);
        let mut config = harness
            .config_with_model(Some(requested_model.clone()))
            .await;
        if provider_mismatched {
            config.model_provider_id = "other-provider".to_string();
        }

        let (model, model_info) =
            assert_resolution_blocks(&harness, config, /*fallback*/ false).await;

        assert_eq!(model, requested_model);
        assert_eq!(model_info.slug, requested_model);
        assert!(!model_info.used_fallback_model_metadata);
    }
}

#[tokio::test]
async fn fallback_resolution_and_missing_model_wait_for_catalog_refresh() {
    let fallback_harness = ModelResolutionHarness::new();
    let fallback_config = fallback_harness
        .config_with_model(Some(fallback_harness.known_model.clone()))
        .await;
    let (fallback_model, _) =
        assert_resolution_blocks(&fallback_harness, fallback_config, /*fallback*/ true).await;
    assert_eq!(fallback_model, fallback_harness.known_model);

    let default_harness = ModelResolutionHarness::new();
    let default_config = default_harness.config_with_model(None).await;
    let (default_model, _) =
        assert_resolution_blocks(&default_harness, default_config, /*fallback*/ false).await;
    assert!(!default_model.is_empty());
}
