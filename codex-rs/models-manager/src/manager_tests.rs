use super::*;
use crate::ModelsManagerConfig;
use chrono::Utc;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::ExternalAuth;
use codex_login::ExternalAuthRefreshContext;
use codex_login::TokenData;
use codex_protocol::auth::AuthMode;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::tempdir;
use tokio::sync::Semaphore;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;

#[path = "model_info_overrides_tests.rs"]
mod model_info_overrides_tests;

const DEFAULT_HTTP_CLIENT_FACTORY: HttpClientFactory =
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);

fn remote_model(slug: &str, display: &str, priority: i32) -> ModelInfo {
    remote_model_with_visibility(slug, display, priority, "list")
}

fn remote_model_with_visibility(
    slug: &str,
    display: &str,
    priority: i32,
    visibility: &str,
) -> ModelInfo {
    serde_json::from_value(json!({
            "slug": slug,
            "display_name": display,
            "description": format!("{display} desc"),
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "low", "description": "low"}, {"effort": "medium", "description": "medium"}],
            "shell_type": "shell_command",
            "visibility": visibility,
            "minimal_client_version": [0, 1, 0],
            "supported_in_api": true,
            "priority": priority,
            "upgrade": null,
            "base_instructions": "base instructions",
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10_000},
            "supports_parallel_tool_calls": false,
            "supports_image_detail_original": false,
            "context_window": 272_000,
            "max_context_window": 272_000,
            "experimental_supported_tools": [],
        }))
        .expect("valid model")
}

fn assert_models_contain(actual: &[ModelInfo], expected: &[ModelInfo]) {
    for model in expected {
        assert!(
            actual.iter().any(|candidate| candidate.slug == model.slug),
            "expected model {} in cached list",
            model.slug
        );
    }
}

#[derive(Debug)]
struct TestModelsEndpoint {
    has_command_auth: bool,
    uses_codex_backend: bool,
    responses: Mutex<VecDeque<Vec<ModelInfo>>>,
    fetch_count: AtomicUsize,
    observed_proxy_policy: Mutex<Option<OutboundProxyPolicy>>,
}

impl TestModelsEndpoint {
    fn new(responses: Vec<Vec<ModelInfo>>) -> Arc<Self> {
        Arc::new(Self {
            has_command_auth: false,
            uses_codex_backend: true,
            responses: Mutex::new(responses.into()),
            fetch_count: AtomicUsize::new(0),
            observed_proxy_policy: Mutex::new(None),
        })
    }

    fn without_refresh(responses: Vec<Vec<ModelInfo>>) -> Arc<Self> {
        Arc::new(Self {
            has_command_auth: false,
            uses_codex_backend: false,
            responses: Mutex::new(responses.into()),
            fetch_count: AtomicUsize::new(0),
            observed_proxy_policy: Mutex::new(None),
        })
    }

    fn fetch_count(&self) -> usize {
        self.fetch_count.load(Ordering::SeqCst)
    }

    fn observed_proxy_policy(&self) -> Option<OutboundProxyPolicy> {
        *self
            .observed_proxy_policy
            .lock()
            .expect("observed proxy policy lock should not be poisoned")
    }

    async fn list_models(&self) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        self.fetch_count.fetch_add(1, Ordering::SeqCst);
        let models = self
            .responses
            .lock()
            .expect("responses lock should not be poisoned")
            .pop_front()
            .unwrap_or_default();
        Ok((models, None))
    }
}

#[derive(Debug)]
struct TestExternalApiKeyAuth;

impl ExternalAuth for TestExternalApiKeyAuth {
    fn resolve(&self) -> codex_login::ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(CodexAuth::from_api_key("test-external-api-key")) })
    }

    fn refresh(
        &self,
        _context: ExternalAuthRefreshContext,
    ) -> codex_login::ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(CodexAuth::from_api_key("test-external-api-key")) })
    }
}

#[derive(Debug)]
struct TestUnresolvedExternalApiKeyAuth;

impl ExternalAuth for TestUnresolvedExternalApiKeyAuth {
    fn resolve(&self) -> codex_login::ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Err(std::io::Error::other("unresolved test auth")) })
    }

    fn refresh(
        &self,
        _context: ExternalAuthRefreshContext,
    ) -> codex_login::ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Err(std::io::Error::other("unresolved test auth")) })
    }
}

impl ModelsEndpointClient for TestModelsEndpoint {
    fn has_command_auth(&self) -> bool {
        self.has_command_auth
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { self.uses_codex_backend })
    }

    fn list_models<'a>(
        &'a self,
        _client_version: &'a str,
        http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>> {
        Box::pin(async move {
            *self
                .observed_proxy_policy
                .lock()
                .expect("observed proxy policy lock should not be poisoned") =
                Some(http_client_factory.outbound_proxy_policy());
            TestModelsEndpoint::list_models(self).await
        })
    }
}

#[derive(Debug)]
enum ControlledResponse {
    Models(Vec<ModelInfo>, Option<String>),
    Failure,
    Panic,
}

#[derive(Debug)]
struct ControlledModelsEndpoint {
    responses: Mutex<VecDeque<ControlledResponse>>,
    fetch_count: AtomicUsize,
    release: Semaphore,
}

impl ControlledModelsEndpoint {
    fn new(responses: Vec<ControlledResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            fetch_count: AtomicUsize::new(0),
            release: Semaphore::new(0),
        })
    }

    async fn wait_for_fetches(&self, expected: usize) {
        for _ in 0..100 {
            if self.fetch_count.load(Ordering::SeqCst) >= expected {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {expected} model fetches");
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }
}

impl ModelsEndpointClient for ControlledModelsEndpoint {
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
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("controlled endpoint should remain open")
                .forget();
            let response = self
                .responses
                .lock()
                .expect("responses lock should not be poisoned")
                .pop_front()
                .expect("controlled response");
            match response {
                ControlledResponse::Models(models, etag) => Ok((models, etag)),
                ControlledResponse::Failure => {
                    Err(std::io::Error::other("controlled model failure").into())
                }
                ControlledResponse::Panic => panic!("controlled model panic"),
            }
        })
    }
}

#[derive(Debug, Default)]
struct NotModifiedModelsEndpoint {
    observed_etags: Mutex<Vec<Option<String>>>,
}

impl ModelsEndpointClient for NotModifiedModelsEndpoint {
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
        Box::pin(async { panic!("conditional endpoint should use list_models_conditional") })
    }

    fn list_models_conditional<'a>(
        &'a self,
        _client_version: &'a str,
        _http_client_factory: HttpClientFactory,
        etag: Option<&'a str>,
    ) -> ModelsEndpointFuture<'a, CoreResult<ModelsFetchResult>> {
        Box::pin(async move {
            self.observed_etags
                .lock()
                .expect("observed ETags lock should not be poisoned")
                .push(etag.map(ToString::to_string));
            Ok(ModelsFetchResult::NotModified)
        })
    }
}

#[tokio::test]
async fn etag_notices_are_non_blocking_coalesced_latest_wins_and_are_waitable() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = ControlledModelsEndpoint::new(vec![
        ControlledResponse::Models(
            vec![remote_model("stale-etag-model", "Stale", 1)],
            Some("etag-a".to_string()),
        ),
        ControlledResponse::Models(
            vec![remote_model("latest-etag-model", "Latest", 1)],
            Some("etag-b".to_string()),
        ),
    ]);
    let manager = Arc::new(openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
    ));

    let _first_refresh =
        Arc::clone(&manager).notify_etag("notice-a".to_string(), DEFAULT_HTTP_CLIENT_FACTORY);
    endpoint.wait_for_fetches(1).await;
    let _duplicate_refresh =
        Arc::clone(&manager).notify_etag("notice-a".to_string(), DEFAULT_HTTP_CLIENT_FACTORY);
    let latest_refresh =
        Arc::clone(&manager).notify_etag("notice-b".to_string(), DEFAULT_HTTP_CLIENT_FACTORY);
    endpoint.release_one();
    endpoint.wait_for_fetches(2).await;
    assert!(
        !manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == "stale-etag-model")
    );

    endpoint.release_one();
    latest_refresh.await;
    let models = manager.get_remote_models().await;
    assert!(models.iter().any(|model| model.slug == "latest-etag-model"));
    assert!(!models.iter().any(|model| model.slug == "stale-etag-model"));
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        manager
            .get_default_model(
                &None,
                false,
                RefreshStrategy::Offline,
                DEFAULT_HTTP_CLIENT_FACTORY,
            )
            .await
            .expect("default model"),
        "latest-etag-model"
    );
}

#[tokio::test]
async fn etag_refresh_failure_preserves_catalog_and_can_retry() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = ControlledModelsEndpoint::new(vec![
        ControlledResponse::Failure,
        ControlledResponse::Models(
            vec![remote_model("retried-etag-model", "Retried", 1)],
            Some("etag-retried".to_string()),
        ),
    ]);
    let manager = Arc::new(openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
    ));
    let cache_identity = manager.ensure_current_cache_identity().await;
    assert!(
        manager
            .apply_remote_models_and_etag_for_identity(
                vec![remote_model("preserved-model", "Preserved", 1)],
                None,
                &cache_identity,
            )
            .await
    );

    let failed_refresh =
        Arc::clone(&manager).notify_etag("notice".to_string(), DEFAULT_HTTP_CLIENT_FACTORY);
    endpoint.wait_for_fetches(1).await;
    endpoint.release_one();
    failed_refresh.await;
    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == "preserved-model")
    );

    let retry_refresh =
        Arc::clone(&manager).notify_etag("notice".to_string(), DEFAULT_HTTP_CLIENT_FACTORY);
    endpoint.wait_for_fetches(2).await;
    endpoint.release_one();
    retry_refresh.await;
    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == "retried-etag-model")
    );
}

#[tokio::test]
async fn etag_refresh_worker_recovers_after_abnormal_exit() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = ControlledModelsEndpoint::new(vec![
        ControlledResponse::Panic,
        ControlledResponse::Models(
            vec![remote_model("recovered-etag-model", "Recovered", 1)],
            Some("etag-recovered".to_string()),
        ),
    ]);
    let manager = Arc::new(openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
    ));

    let failed_refresh =
        Arc::clone(&manager).notify_etag("notice".to_string(), DEFAULT_HTTP_CLIENT_FACTORY);
    endpoint.wait_for_fetches(1).await;
    endpoint.release_one();
    timeout(Duration::from_secs(1), failed_refresh)
        .await
        .expect("waiters should be released after an abnormal worker exit");

    let retry_refresh =
        Arc::clone(&manager).notify_etag("notice".to_string(), DEFAULT_HTTP_CLIENT_FACTORY);
    endpoint.wait_for_fetches(2).await;
    endpoint.release_one();
    timeout(Duration::from_secs(1), retry_refresh)
        .await
        .expect("a later notice should start a replacement worker");

    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == "recovered-etag-model")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn older_openai_manager_fetch_cannot_overwrite_newer_process_cache() {
    let codex_home = tempdir().expect("temp dir");
    let older_endpoint = ControlledModelsEndpoint::new(vec![ControlledResponse::Models(
        vec![remote_model("older-process-model", "Older", 1)],
        Some("older-process-etag".to_string()),
    )]);
    let newer_endpoint = ControlledModelsEndpoint::new(vec![ControlledResponse::Models(
        vec![remote_model("newer-process-model", "Newer", 1)],
        Some("newer-process-etag".to_string()),
    )]);
    let older = Arc::new(openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        older_endpoint.clone(),
    ));
    let newer = Arc::new(openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        newer_endpoint.clone(),
    ));

    let older_refresh = tokio::spawn({
        let older = Arc::clone(&older);
        async move {
            older
                .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
                .await
        }
    });
    older_endpoint.wait_for_fetches(1).await;
    let newer_refresh = tokio::spawn({
        let newer = Arc::clone(&newer);
        async move {
            newer
                .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
                .await
        }
    });
    newer_endpoint.wait_for_fetches(1).await;
    newer_endpoint.release_one();
    newer_refresh
        .await
        .expect("newer refresh task should complete")
        .expect("newer refresh should succeed");
    older_endpoint.release_one();
    older_refresh
        .await
        .expect("older refresh task should complete")
        .expect("older refresh should succeed without replacing the newer cache");

    let verifier = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );
    assert!(verifier.try_load_cache().await.expect("load cache"));
    let persisted = verifier.get_remote_models().await;
    assert!(
        persisted
            .iter()
            .any(|model| model.slug == "newer-process-model")
    );
    assert!(
        !persisted
            .iter()
            .any(|model| model.slug == "older-process-model")
    );
    assert_eq!(
        verifier.get_etag().await.as_deref(),
        Some("newer-process-etag")
    );
}

#[tokio::test]
async fn matching_etag_renews_ttl_without_fetching() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = ControlledModelsEndpoint::new(Vec::new());
    let manager = Arc::new(openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
    ));
    let cached = vec![remote_model("cached-etag-model", "Cached", 1)];
    manager
        .cache_manager
        .persist_cache(
            &cached,
            Some("same-etag".to_string()),
            crate::client_version_to_whole(),
        )
        .await;
    manager
        .cache_manager
        .manipulate_cache_for_test(|fetched_at| {
            *fetched_at = chrono::Utc::now() - chrono::Duration::hours(1);
        })
        .await
        .expect("age cache");
    manager.state.write().await.etag = Some("same-etag".to_string());

    Arc::clone(&manager)
        .notify_etag("same-etag".to_string(), DEFAULT_HTTP_CLIENT_FACTORY)
        .await;

    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 0);
    assert!(
        manager
            .cache_manager
            .load_fresh(&crate::client_version_to_whole())
            .await
            .expect("read cache")
            .is_some()
    );
}

#[tokio::test]
async fn online_refresh_revalidates_stale_cache_with_its_etag() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = Arc::new(NotModifiedModelsEndpoint::default());
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());
    let cached = vec![remote_model("revalidated-model", "Revalidated", 1)];
    manager
        .cache_manager
        .persist_cache(
            &cached,
            Some("cache-etag".to_string()),
            crate::client_version_to_whole(),
        )
        .await;
    assert!(manager.try_load_cache().await.expect("load cache"));
    manager
        .cache_manager
        .manipulate_cache_for_test(|fetched_at| {
            *fetched_at = chrono::Utc::now() - chrono::Duration::hours(1);
        })
        .await
        .expect("age cache");

    manager
        .raw_model_catalog(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("refresh catalog");

    assert_eq!(
        *endpoint
            .observed_etags
            .lock()
            .expect("observed ETags lock should not be poisoned"),
        vec![Some("cache-etag".to_string())]
    );
    assert!(
        manager
            .cache_manager
            .load_fresh(&crate::client_version_to_whole())
            .await
            .expect("read cache")
            .is_some()
    );
    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == "revalidated-model")
    );
}

#[tokio::test]
async fn online_list_models_reports_refresh_failure_instead_of_stale_success() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = ControlledModelsEndpoint::new(vec![ControlledResponse::Failure]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());
    endpoint.release_one();

    let error = manager
        .list_models(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect_err("an online refresh failure must be observable by the caller");

    assert!(error.to_string().contains("controlled model failure"));
    assert_eq!(endpoint.fetch_count.load(Ordering::SeqCst), 1);
}

fn openai_manager_for_tests(
    codex_home: std::path::PathBuf,
    endpoint_client: Arc<dyn ModelsEndpointClient>,
) -> OpenAiModelsManager {
    openai_manager_for_tests_with_auth(
        codex_home,
        endpoint_client,
        Some(AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        )),
    )
}

fn openai_manager_for_tests_with_auth(
    codex_home: std::path::PathBuf,
    endpoint_client: Arc<dyn ModelsEndpointClient>,
    auth_manager: Option<Arc<AuthManager>>,
) -> OpenAiModelsManager {
    OpenAiModelsManager::new(
        codex_home,
        endpoint_client,
        auth_manager,
        Arc::new(|| "test-provider-identity".to_string()),
    )
}

#[tokio::test]
async fn offline_refresh_checks_cache_identity_once_before_the_cache_read() {
    let codex_home = tempdir().expect("temp dir");
    let identity_reads = Arc::new(AtomicUsize::new(0));
    let identity_reads_for_cache = Arc::clone(&identity_reads);
    let manager = OpenAiModelsManager::new(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::without_refresh(Vec::new()),
        None,
        Arc::new(move || {
            identity_reads_for_cache.fetch_add(1, Ordering::SeqCst);
            "counted-provider-identity".to_string()
        }),
    );
    identity_reads.store(0, Ordering::SeqCst);

    manager
        .refresh_available_models(RefreshStrategy::Offline, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("offline cache refresh should succeed");

    assert_eq!(identity_reads.load(Ordering::SeqCst), 2);
}

fn static_manager_for_tests(model_catalog: ModelsResponse) -> StaticModelsManager {
    StaticModelsManager::new(/*auth_manager*/ None, model_catalog)
}

async fn chatgpt_auth_tokens_for_tests(codex_home: &Path) -> CodexAuth {
    let auth_dot_json = codex_login::AuthDotJson {
        auth_mode: Some(AuthMode::ChatgptAuthTokens),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: codex_login::token_data::parse_chatgpt_jwt_claims(
                "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.\
eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9wbGFuX3R5cGUiOiJwcm8iLCJjaGF0Z3B0X3VzZXJfaWQiOiJ1c2VyLWlkIiwiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC1pZCJ9fQ.\
c2ln",
            )
            .expect("fake id token should parse"),
            access_token: "Access Token".to_string(),
            refresh_token: "test".to_string(),
            account_id: Some("account_id".to_string()),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };
    std::fs::create_dir_all(codex_home).expect("codex home should be created");
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::to_string(&auth_dot_json).expect("auth should serialize"),
    )
    .expect("auth.json should be written");

    CodexAuth::from_auth_storage(
        codex_home,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        &codex_login::test_support::transport_default_auth_route_config(),
    )
    .await
    .expect("auth should load")
    .expect("auth should be present")
}

#[tokio::test]
async fn static_manager_preserves_supported_requested_model_when_fallback_is_allowed() {
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![
            remote_model("provider-default", "Default", /*priority*/ 0),
            remote_model("provider-supported", "Supported", /*priority*/ 1),
        ],
    });
    let requested_model = Some("provider-supported".to_string());

    let model = manager
        .get_default_model(
            &requested_model,
            /*allow_provider_model_fallback*/ true,
            RefreshStrategy::Offline,
            DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("default model");

    assert_eq!(model, "provider-supported");
}

#[tokio::test]
async fn static_manager_falls_back_from_unsupported_requested_model_when_allowed() {
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![
            remote_model("provider-default", "Default", /*priority*/ 0),
            remote_model("provider-supported", "Supported", /*priority*/ 1),
        ],
    });
    let requested_model = Some("unsupported".to_string());

    let model = manager
        .get_default_model(
            &requested_model,
            /*allow_provider_model_fallback*/ true,
            RefreshStrategy::Offline,
            DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("default model");

    assert_eq!(model, "provider-default");
}

#[tokio::test]
async fn static_manager_preserves_requested_sol_when_fallback_is_allowed() {
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![remote_model(
            "provider-default",
            "Default",
            /*priority*/ 0,
        )],
    });

    for requested_model in ["gpt-5.6-sol", "openai.gpt-5.6-sol"] {
        let model = manager
            .get_default_model(
                &Some(requested_model.to_string()),
                /*allow_provider_model_fallback*/ true,
                RefreshStrategy::Offline,
                DEFAULT_HTTP_CLIENT_FACTORY,
            )
            .await
            .expect("default model");

        assert_eq!(model, requested_model);
    }
}

#[tokio::test]
async fn static_manager_falls_back_from_unqualified_sol_suffixes() {
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![remote_model(
            "provider-default",
            "Default",
            /*priority*/ 0,
        )],
    });

    for requested_model in ["notgpt-5.6-sol", ".gpt-5.6-sol"] {
        let model = manager
            .get_default_model(
                &Some(requested_model.to_string()),
                /*allow_provider_model_fallback*/ true,
                RefreshStrategy::Offline,
                DEFAULT_HTTP_CLIENT_FACTORY,
            )
            .await
            .expect("default model");

        assert_eq!(model, "provider-default");
    }
}

#[tokio::test]
async fn static_manager_preserves_unsupported_requested_model_when_fallback_is_disabled() {
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![remote_model(
            "provider-default",
            "Default",
            /*priority*/ 0,
        )],
    });
    let requested_model = Some("unsupported".to_string());

    let model = manager
        .get_default_model(
            &requested_model,
            /*allow_provider_model_fallback*/ false,
            RefreshStrategy::Offline,
            DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("default model");

    assert_eq!(model, "unsupported");
    assert_eq!(manager.list_models_call_count(), 0);
}

#[tokio::test]
async fn static_manager_rejects_fallback_when_catalog_is_empty() {
    let manager = static_manager_for_tests(ModelsResponse { models: Vec::new() });
    let requested_model = Some("unsupported".to_string());

    let error = manager
        .get_default_model(
            &requested_model,
            /*allow_provider_model_fallback*/ true,
            RefreshStrategy::Offline,
            DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect_err("an empty catalog must not select an empty model identifier");

    assert!(
        error
            .to_string()
            .contains("does not contain a usable model")
    );
}

#[test]
#[should_panic(expected = "bundled model catalog must load")]
fn bundled_catalog_load_errors_are_not_replaced_with_an_empty_catalog() {
    let _ = require_bundled_models(Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid bundled catalog",
    )));
}

#[tokio::test]
async fn dynamic_manager_preserves_requested_model_when_fallback_is_allowed() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(Vec::new());
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());
    let requested_model = Some("unsupported".to_string());

    let model = manager
        .get_default_model(
            &requested_model,
            /*allow_provider_model_fallback*/ true,
            RefreshStrategy::Online,
            DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("default model");

    assert_eq!(model, "unsupported");
    assert_eq!(endpoint.fetch_count(), 0);
}

#[tokio::test]
async fn get_model_info_tracks_fallback_usage() {
    let codex_home = tempdir().expect("temp dir");
    let config = ModelsManagerConfig::default();
    let manager = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );
    let known_slug = manager
        .get_remote_models()
        .await
        .first()
        .expect("bundled models should include at least one model")
        .slug
        .clone();

    let known = manager.get_model_info(known_slug.as_str(), &config).await;
    assert!(!known.used_fallback_model_metadata);
    assert_eq!(known.slug, known_slug);

    let unknown = manager
        .get_model_info("model-that-does-not-exist", &config)
        .await;
    assert!(unknown.used_fallback_model_metadata);
    assert_eq!(unknown.slug, "model-that-does-not-exist");
}

#[tokio::test]
async fn get_model_info_uses_custom_catalog() {
    let config = ModelsManagerConfig::default();
    let mut overlay = remote_model("gpt-overlay", "Overlay", /*priority*/ 0);
    overlay.supports_image_detail_original = true;

    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![overlay],
    });

    let model_info = manager
        .get_model_info("gpt-overlay-experiment", &config)
        .await;

    assert_eq!(model_info.slug, "gpt-overlay-experiment");
    assert_eq!(model_info.display_name, "Overlay");
    assert_eq!(model_info.context_window, Some(272_000));
    assert!(model_info.supports_image_detail_original);
    assert!(!model_info.supports_parallel_tool_calls);
    assert!(!model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn exact_gpt_5_2_codex_alias_applies_local_personality_to_catalog_match() {
    let config = ModelsManagerConfig {
        personality_enabled: true,
        ..Default::default()
    };
    let remote = remote_model("gpt-5.2", "GPT-5.2", /*priority*/ 0);
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![remote],
    });

    let model_info = manager.get_model_info("gpt-5.2-codex", &config).await;

    assert!(!model_info.used_fallback_model_metadata);
    assert_eq!(model_info.slug, "gpt-5.2-codex");
    assert!(
        model_info
            .get_model_instructions(Some(codex_protocol::config_types::Personality::Friendly,))
            .contains("supportive teammate")
    );
}

#[tokio::test]
async fn get_model_info_rejects_unstructured_prefix_collision() {
    let config = ModelsManagerConfig::default();
    let mut overlay = remote_model("gpt-overlay", "Overlay", /*priority*/ 0);
    overlay.supports_image_detail_original = true;
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![overlay],
    });

    let model_info = manager.get_model_info("gpt-overlayevil", &config).await;

    assert_eq!(model_info.slug, "gpt-overlayevil");
    assert_eq!(model_info.display_name, "gpt-overlayevil");
    assert!(!model_info.supports_image_detail_original);
    assert!(model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn get_model_info_matches_namespaced_suffix() {
    let config = ModelsManagerConfig::default();
    let mut remote = remote_model("gpt-image", "Image", /*priority*/ 0);
    remote.supports_image_detail_original = true;
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![remote],
    });
    let namespaced_model = "custom/gpt-image".to_string();

    let model_info = manager.get_model_info(&namespaced_model, &config).await;

    assert_eq!(model_info.slug, namespaced_model);
    assert!(model_info.supports_image_detail_original);
    assert!(!model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn get_model_info_matches_hyphenated_provider_namespace_suffix() {
    let config = ModelsManagerConfig::default();
    let remote = remote_model("gpt-image", "Image", /*priority*/ 0);
    let manager = static_manager_for_tests(ModelsResponse {
        models: vec![remote],
    });
    let namespaced_model = "openai-codex/gpt-image".to_string();

    let model_info = manager.get_model_info(&namespaced_model, &config).await;

    assert_eq!(model_info.slug, namespaced_model);
    assert!(!model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn get_model_info_rejects_multi_segment_namespace_suffix_matching() {
    let codex_home = tempdir().expect("temp dir");
    let config = ModelsManagerConfig::default();
    let manager = openai_manager_for_tests(
        codex_home.path().to_path_buf(),
        TestModelsEndpoint::new(Vec::new()),
    );
    let known_slug = manager
        .get_remote_models()
        .await
        .first()
        .expect("bundled models should include at least one model")
        .slug
        .clone();
    let namespaced_model = format!("ns1/ns2/{known_slug}");

    let model_info = manager.get_model_info(&namespaced_model, &config).await;

    assert_eq!(model_info.slug, namespaced_model);
    assert!(model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn refresh_available_models_sorts_by_priority() {
    let remote_models = vec![
        remote_model("priority-low", "Low", /*priority*/ 1),
        remote_model("priority-high", "High", /*priority*/ 0),
    ];
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![remote_models.clone()]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());

    let available = manager
        .list_models(
            RefreshStrategy::Online,
            HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
        )
        .await
        .expect("list models");
    assert_models_contain(&manager.get_remote_models().await, &remote_models);
    assert_eq!(
        endpoint.observed_proxy_policy(),
        Some(OutboundProxyPolicy::RespectSystemProxy)
    );
    let high_idx = available
        .iter()
        .position(|model| model.model == "priority-high")
        .expect("priority-high should be listed");
    let low_idx = available
        .iter()
        .position(|model| model.model == "priority-low")
        .expect("priority-low should be listed");
    assert!(
        high_idx < low_idx,
        "higher priority should be listed before lower priority"
    );
    assert_eq!(endpoint.fetch_count(), 1, "expected a single model fetch");
}

#[tokio::test]
async fn picker_snapshot_is_reused_until_the_catalog_changes() {
    let remote_models = vec![remote_model(
        "shared-picker-snapshot",
        "Shared Picker Snapshot",
        /*priority*/ 0,
    )];
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![remote_models]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint);

    let initial = manager
        .try_list_models_shared()
        .expect("initial picker snapshot should be available");
    let repeated = manager
        .list_models_shared(RefreshStrategy::Offline, DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("list models");
    assert!(Arc::ptr_eq(&initial, &repeated));

    let initial_slug = initial
        .first()
        .expect("bundled catalog should have a model")
        .model
        .clone();
    manager
        .get_model_info(&initial_slug, &ModelsManagerConfig::default())
        .await;
    let after_lookup = manager
        .try_list_models_shared()
        .expect("model lookup should leave the picker snapshot available");
    assert!(Arc::ptr_eq(&initial, &after_lookup));

    let refreshed = manager
        .list_models_shared(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("refresh models");
    assert!(!Arc::ptr_eq(&initial, &refreshed));
    assert!(
        refreshed
            .iter()
            .any(|model| model.model == "shared-picker-snapshot")
    );
    let refreshed_again = manager
        .list_models_shared(RefreshStrategy::Offline, DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("list models");
    assert!(Arc::ptr_eq(&refreshed, &refreshed_again));
}

#[tokio::test]
async fn refresh_available_models_uses_remote_only_catalog_for_chatgpt_auth() {
    let remote_models = vec![remote_model(
        "chatgpt-visible-source-of-truth",
        "ChatGPT Visible",
        /*priority*/ 0,
    )];
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![remote_models.clone()]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("refresh succeeds");

    assert_eq!(manager.get_remote_models().await, remote_models);
    assert_eq!(endpoint.fetch_count(), 1, "expected a single model fetch");
}

#[tokio::test]
async fn refresh_available_models_uses_cached_remote_only_catalog_for_chatgpt_auth() {
    let remote_models = vec![remote_model(
        "chatgpt-cached-source-of-truth",
        "ChatGPT Cached",
        /*priority*/ 0,
    )];
    let codex_home = tempdir().expect("temp dir");
    let fetch_endpoint = TestModelsEndpoint::new(vec![remote_models.clone()]);
    let fetch_manager =
        openai_manager_for_tests(codex_home.path().to_path_buf(), fetch_endpoint.clone());

    fetch_manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("initial refresh succeeds");

    let cache_endpoint = TestModelsEndpoint::new(Vec::new());
    let cache_manager =
        openai_manager_for_tests(codex_home.path().to_path_buf(), cache_endpoint.clone());

    cache_manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("cached refresh succeeds");

    assert_eq!(cache_manager.get_remote_models().await, remote_models);
    assert_eq!(
        cache_endpoint.fetch_count(),
        0,
        "fresh cache should avoid a model fetch"
    );
}

#[tokio::test]
async fn get_model_info_uses_fallback_for_bundled_models_when_chatgpt_remote_is_authoritative() {
    let remote_models = vec![remote_model(
        "chatgpt-authoritative-model-info",
        "ChatGPT Model Info",
        /*priority*/ 0,
    )];
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![remote_models]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint);
    let bundled_slug = load_bundled_models()
        .expect("bundled models should parse")
        .first()
        .expect("bundled models should contain at least one model")
        .slug
        .clone();

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("refresh succeeds");

    let model_info = manager
        .get_model_info(&bundled_slug, &ModelsManagerConfig::default())
        .await;

    assert_eq!(model_info.slug, bundled_slug);
    assert!(model_info.used_fallback_model_metadata);
}

#[tokio::test]
async fn refresh_available_models_preserves_bundled_catalog_for_empty_chatgpt_remote() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![Vec::new()]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint);
    let expected = load_bundled_models().expect("bundled models should parse");

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("refresh succeeds");

    assert_eq!(manager.get_remote_models().await, expected);
}

#[tokio::test]
async fn refresh_available_models_merges_hidden_only_chatgpt_remote_with_bundled_catalog() {
    let hidden_remote = remote_model_with_visibility(
        "chatgpt-hidden-only",
        "ChatGPT Hidden",
        /*priority*/ 0,
        "hide",
    );
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![vec![hidden_remote.clone()]]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint);
    let mut expected = load_bundled_models().expect("bundled models should parse");
    expected.push(hidden_remote);

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("refresh succeeds");

    assert_eq!(manager.get_remote_models().await, expected);
}

#[tokio::test]
async fn refresh_available_models_keeps_merging_for_api_auth() {
    let remote_models = vec![remote_model(
        "api-auth-visible-remote",
        "API Auth Visible",
        /*priority*/ 0,
    )];
    let codex_home = tempdir().expect("temp dir");
    let endpoint = Arc::new(TestModelsEndpoint {
        has_command_auth: true,
        uses_codex_backend: false,
        responses: Mutex::new(vec![remote_models.clone()].into()),
        fetch_count: AtomicUsize::new(0),
        observed_proxy_policy: Mutex::new(None),
    });
    let manager = openai_manager_for_tests_with_auth(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
            "test-api-key",
        ))),
    );
    let mut expected = load_bundled_models().expect("bundled models should parse");
    expected.extend(remote_models);

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("refresh succeeds");

    assert_eq!(manager.get_remote_models().await, expected);
    assert_eq!(endpoint.fetch_count(), 1, "expected a single model fetch");
}

#[tokio::test]
async fn refresh_available_models_uses_cache_when_fresh() {
    let remote_models = vec![remote_model("cached", "Cached", /*priority*/ 5)];
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![remote_models.clone()]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("first refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &remote_models);

    // Second call should read from cache and avoid the network.
    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("cached refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &remote_models);
    assert_eq!(
        endpoint.fetch_count(),
        1,
        "cache hit should avoid a second model fetch"
    );
}

#[tokio::test]
async fn runtime_identity_change_does_not_reuse_disk_or_in_memory_catalog() {
    let codex_home = tempdir().expect("temp dir");
    let first_model = remote_model("first-account-model", "First Account", 1);
    let second_model = remote_model("second-account-model", "Second Account", 1);
    let endpoint = TestModelsEndpoint::new(vec![vec![first_model], vec![second_model.clone()]]);
    let identity = Arc::new(Mutex::new("scope-digest-one".to_string()));
    let identity_for_cache = Arc::clone(&identity);
    let manager = OpenAiModelsManager::new(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        Some(AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        )),
        Arc::new(move || {
            identity_for_cache
                .lock()
                .expect("identity lock should not be poisoned")
                .clone()
        }),
    );

    manager
        .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("first account refresh should succeed");
    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == "first-account-model")
    );

    *identity
        .lock()
        .expect("identity lock should not be poisoned") = "scope-digest-two".to_string();
    manager
        .refresh_available_models(RefreshStrategy::Offline, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("offline identity transition should fail closed");
    assert!(
        !manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == "first-account-model")
    );

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("second account refresh should fetch its own catalog");
    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == second_model.slug)
    );
    assert_eq!(endpoint.fetch_count(), 2);
}

#[tokio::test]
async fn direct_catalog_reads_reset_state_after_runtime_identity_change() {
    let codex_home = tempdir().expect("temp dir");
    let first_model = remote_model("direct-read-first", "Direct Read First", 1);
    let endpoint = TestModelsEndpoint::new(vec![vec![first_model.clone()]]);
    let identity = Arc::new(Mutex::new("direct-read-scope-one".to_string()));
    let identity_for_cache = Arc::clone(&identity);
    let manager = OpenAiModelsManager::new(
        codex_home.path().to_path_buf(),
        endpoint,
        Some(AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        )),
        Arc::new(move || {
            identity_for_cache
                .lock()
                .expect("identity lock should not be poisoned")
                .clone()
        }),
    );

    manager
        .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("initial refresh should succeed");
    assert!(
        !manager
            .get_model_info(&first_model.slug, &ModelsManagerConfig::default())
            .await
            .used_fallback_model_metadata
    );

    *identity
        .lock()
        .expect("identity lock should not be poisoned") = "direct-read-scope-two".to_string();
    assert!(
        manager
            .get_model_info(&first_model.slug, &ModelsManagerConfig::default())
            .await
            .used_fallback_model_metadata,
        "model lookup must not use the previous identity's catalog"
    );

    let second_model = remote_model("direct-read-second", "Direct Read Second", 1);
    let second_identity = manager.ensure_current_cache_identity().await;
    assert!(
        manager
            .apply_remote_models_and_etag_for_identity(
                vec![second_model.clone()],
                None,
                &second_identity,
            )
            .await
    );
    *identity
        .lock()
        .expect("identity lock should not be poisoned") = "direct-read-scope-three".to_string();
    assert!(
        !manager
            .get_remote_models()
            .await
            .iter()
            .any(|model| model.slug == second_model.slug),
        "async catalog reads must not use the previous identity's catalog"
    );

    let third_model = remote_model("direct-read-third", "Direct Read Third", 1);
    let third_identity = manager.ensure_current_cache_identity().await;
    assert!(
        manager
            .apply_remote_models_and_etag_for_identity(
                vec![third_model.clone()],
                None,
                &third_identity,
            )
            .await
    );
    *identity
        .lock()
        .expect("identity lock should not be poisoned") = "direct-read-scope-four".to_string();
    assert!(
        !manager
            .try_get_remote_models()
            .expect("model state should not be locked")
            .iter()
            .any(|model| model.slug == third_model.slug),
        "synchronous catalog reads must not use the previous identity's catalog"
    );

    let fourth_model = remote_model("direct-read-fourth", "Direct Read Fourth", 1);
    let fourth_identity = manager.ensure_current_cache_identity().await;
    assert!(
        manager
            .apply_remote_models_and_etag_for_identity(
                vec![fourth_model.clone()],
                None,
                &fourth_identity,
            )
            .await
    );
    *identity
        .lock()
        .expect("identity lock should not be poisoned") = "direct-read-scope-five".to_string();
    assert!(
        !manager
            .try_list_models_shared()
            .expect("model state should not be locked")
            .iter()
            .any(|model| model.model == fourth_model.slug),
        "synchronous preset reads must not use the previous identity's catalog"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_model_state_access_completes_with_coherent_snapshots() {
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(Vec::new());
    let identity = Arc::new(Mutex::new("scope-initial".to_string()));
    let identity_for_cache = Arc::clone(&identity);
    let manager = Arc::new(OpenAiModelsManager::new(
        codex_home.path().to_path_buf(),
        endpoint,
        Some(AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        )),
        Arc::new(move || {
            identity_for_cache
                .lock()
                .expect("identity lock should not be poisoned")
                .clone()
        }),
    ));

    timeout(Duration::from_secs(5), async {
        let identity_manager = Arc::clone(&manager);
        let identity_writer = Arc::clone(&identity);
        let identity_task = tokio::spawn(async move {
            for index in 0..128 {
                *identity_writer
                    .lock()
                    .expect("identity lock should not be poisoned") =
                    format!("scope-{}", index % 3);
                identity_manager.ensure_current_cache_identity().await;
                tokio::task::yield_now().await;
            }
        });

        let catalog_manager = Arc::clone(&manager);
        let catalog_task = tokio::spawn(async move {
            for index in 0..128 {
                let catalog_identity = catalog_manager.ensure_current_cache_identity().await;
                let _ = catalog_manager
                    .apply_remote_models_and_etag_for_identity(
                        vec![remote_model(
                            &format!("coherent-model-{index}"),
                            "Coherent",
                            index,
                        )],
                        Some(format!("coherent-etag-{index}")),
                        &catalog_identity,
                    )
                    .await;
                tokio::task::yield_now().await;
            }
        });

        let reader_manager = Arc::clone(&manager);
        let reader_task = tokio::spawn(async move {
            for _ in 0..512 {
                {
                    let state = reader_manager.state.read().await;
                    if let Some(index) = state
                        .etag
                        .as_deref()
                        .and_then(|etag| etag.strip_prefix("coherent-etag-"))
                    {
                        let expected_slug = format!("coherent-model-{index}");
                        assert!(
                            state
                                .remote_models
                                .iter()
                                .any(|model| model.slug == expected_slug),
                            "catalog and ETag must come from the same state update"
                        );
                    }
                }

                let _ = reader_manager.get_remote_models().await;
                let _ = reader_manager.get_etag().await;
                let _ = reader_manager.try_get_remote_models();
                tokio::task::yield_now().await;
            }
        });

        identity_task.await.expect("identity task should complete");
        catalog_task.await.expect("catalog task should complete");
        reader_task.await.expect("reader task should complete");
    })
    .await
    .expect("concurrent model state access should not deadlock");

    *identity
        .lock()
        .expect("identity lock should not be poisoned") = "scope-final".to_string();
    let final_identity = manager.ensure_current_cache_identity().await;
    assert!(
        manager
            .apply_remote_models_and_etag_for_identity(
                vec![remote_model("coherent-model-final", "Final", 0)],
                Some("coherent-etag-final".to_string()),
                &final_identity,
            )
            .await
    );

    let state = manager.state.read().await;
    assert_eq!(state.active_cache_identity, "scope-final");
    assert_eq!(state.etag.as_deref(), Some("coherent-etag-final"));
    assert_eq!(state.remote_models.len(), 1);
    assert_eq!(state.remote_models[0].slug, "coherent-model-final");
}

#[tokio::test]
async fn refresh_available_models_refetches_when_cache_stale() {
    let initial_models = vec![remote_model("stale", "Stale", /*priority*/ 1)];
    let codex_home = tempdir().expect("temp dir");
    let updated_models = vec![remote_model("fresh", "Fresh", /*priority*/ 9)];
    let endpoint = TestModelsEndpoint::new(vec![initial_models.clone(), updated_models.clone()]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("initial refresh succeeds");

    // Rewrite cache with an old timestamp so it is treated as stale.
    manager
        .cache_manager
        .manipulate_cache_for_test(|fetched_at| {
            *fetched_at = Utc::now() - chrono::Duration::hours(1);
        })
        .await
        .expect("cache manipulation succeeds");

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("second refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &updated_models);
    assert_eq!(
        endpoint.fetch_count(),
        2,
        "stale cache refresh should fetch models again"
    );
}

#[tokio::test]
async fn refresh_available_models_refetches_when_version_mismatch() {
    let initial_models = vec![remote_model("old", "Old", /*priority*/ 1)];
    let codex_home = tempdir().expect("temp dir");
    let updated_models = vec![remote_model("new", "New", /*priority*/ 2)];
    let endpoint = TestModelsEndpoint::new(vec![initial_models.clone(), updated_models.clone()]);
    let manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("initial refresh succeeds");

    manager
        .cache_manager
        .mutate_cache_for_test(|cache| {
            let client_version = crate::client_version_to_whole();
            cache.client_version = Some(format!("{client_version}-mismatch"));
        })
        .await
        .expect("cache mutation succeeds");

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("second refresh succeeds");
    assert_models_contain(&manager.get_remote_models().await, &updated_models);
    assert_eq!(
        endpoint.fetch_count(),
        2,
        "version mismatch should fetch models again"
    );
}

#[tokio::test]
async fn refresh_available_models_drops_removed_remote_models() {
    let initial_models = vec![remote_model(
        "remote-old",
        "Remote Old",
        /*priority*/ 1,
    )];
    let codex_home = tempdir().expect("temp dir");
    let refreshed_models = vec![remote_model(
        "remote-new",
        "Remote New",
        /*priority*/ 1,
    )];
    let endpoint = TestModelsEndpoint::new(vec![initial_models, refreshed_models]);
    let mut manager = openai_manager_for_tests(codex_home.path().to_path_buf(), endpoint.clone());
    manager.cache_manager.set_ttl(Duration::ZERO);

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("initial refresh succeeds");

    manager
        .refresh_available_models(
            RefreshStrategy::OnlineIfUncached,
            &DEFAULT_HTTP_CLIENT_FACTORY,
        )
        .await
        .expect("second refresh succeeds");

    let available = manager
        .try_list_models()
        .expect("models should be available");
    assert!(
        available.iter().any(|preset| preset.model == "remote-new"),
        "new remote model should be listed"
    );
    assert!(
        !available.iter().any(|preset| preset.model == "remote-old"),
        "removed remote model should not be listed"
    );
    assert_eq!(
        endpoint.fetch_count(),
        2,
        "second refresh should fetch models again"
    );
}

#[tokio::test]
async fn refresh_available_models_skips_network_without_chatgpt_auth() {
    let dynamic_slug = "dynamic-model-only-for-test-noauth";
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::without_refresh(vec![vec![remote_model(
        dynamic_slug,
        "No Auth",
        /*priority*/ 1,
    )]]);
    let manager = openai_manager_for_tests_with_auth(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        /*auth_manager*/ None,
    );

    manager
        .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("refresh should no-op without chatgpt auth");
    let cached_remote = manager.get_remote_models().await;
    assert!(
        !cached_remote
            .iter()
            .any(|candidate| candidate.slug == dynamic_slug),
        "remote refresh should be skipped without chatgpt auth"
    );
    assert_eq!(
        endpoint.fetch_count(),
        0,
        "endpoint that cannot refresh should avoid model fetches"
    );
}

#[derive(Debug)]
struct TestAuthAwareModelsEndpoint {
    auth_manager: Option<Arc<AuthManager>>,
    responses: Mutex<VecDeque<Vec<ModelInfo>>>,
    fetch_count: AtomicUsize,
}

impl TestAuthAwareModelsEndpoint {
    fn new(auth_manager: Option<Arc<AuthManager>>, responses: Vec<Vec<ModelInfo>>) -> Arc<Self> {
        Arc::new(Self {
            auth_manager,
            responses: Mutex::new(responses.into()),
            fetch_count: AtomicUsize::new(0),
        })
    }

    fn fetch_count(&self) -> usize {
        self.fetch_count.load(Ordering::SeqCst)
    }

    async fn uses_codex_backend(&self) -> bool {
        match self.auth_manager.as_ref() {
            Some(auth_manager) => auth_manager
                .auth()
                .await
                .as_ref()
                .is_some_and(CodexAuth::uses_codex_backend),
            None => false,
        }
    }

    async fn list_models(&self) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        self.fetch_count.fetch_add(1, Ordering::SeqCst);
        let models = self
            .responses
            .lock()
            .expect("responses lock should not be poisoned")
            .pop_front()
            .unwrap_or_default();
        Ok((models, None))
    }
}

impl ModelsEndpointClient for TestAuthAwareModelsEndpoint {
    fn has_command_auth(&self) -> bool {
        false
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(TestAuthAwareModelsEndpoint::uses_codex_backend(self))
    }

    fn list_models<'a>(
        &'a self,
        _client_version: &'a str,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>> {
        Box::pin(TestAuthAwareModelsEndpoint::list_models(self))
    }
}

#[tokio::test]
async fn refresh_available_models_skips_network_when_external_api_key_overrides_chatgpt_auth() {
    let dynamic_slug = "dynamic-model-only-for-test-external-api-key";
    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    auth_manager
        .set_external_auth(Arc::new(TestExternalApiKeyAuth))
        .await
        .expect("external API key auth should resolve");
    let endpoint = TestAuthAwareModelsEndpoint::new(
        Some(Arc::clone(&auth_manager)),
        vec![vec![remote_model(
            dynamic_slug,
            "External API Key",
            /*priority*/ 1,
        )]],
    );
    let manager = openai_manager_for_tests_with_auth(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        Some(auth_manager),
    );

    manager
        .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("refresh should no-op with API key auth");
    let cached_remote = manager.get_remote_models().await;

    assert!(
        !cached_remote
            .iter()
            .any(|candidate| candidate.slug == dynamic_slug),
        "remote refresh should be skipped when external API key auth is active"
    );
    assert_eq!(
        endpoint.fetch_count(),
        0,
        "endpoint should avoid model fetches when external API key auth is active"
    );
}

#[tokio::test]
async fn refresh_available_models_uses_cached_chatgpt_when_external_api_key_is_unresolved() {
    let dynamic_slug = "dynamic-model-only-for-test-unresolved-external-api-key";
    let codex_home = tempdir().expect("temp dir");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    auth_manager
        .set_external_auth(Arc::new(TestUnresolvedExternalApiKeyAuth))
        .await
        .expect_err("unresolved external auth should be rejected");
    let endpoint = TestAuthAwareModelsEndpoint::new(
        Some(Arc::clone(&auth_manager)),
        vec![vec![remote_model(
            dynamic_slug,
            "Unresolved External API Key",
            /*priority*/ 1,
        )]],
    );
    let manager = openai_manager_for_tests_with_auth(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        Some(auth_manager),
    );

    manager
        .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("refresh should fall back to cached ChatGPT auth");

    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|candidate| candidate.slug == dynamic_slug),
        "remote refresh should include models fetched with cached ChatGPT auth"
    );
    assert_eq!(
        endpoint.fetch_count(),
        1,
        "endpoint should fetch models when unresolved external API key falls back to ChatGPT auth"
    );
}

#[tokio::test]
async fn refresh_available_models_fetches_with_chatgpt_auth_tokens() {
    let dynamic_slug = "dynamic-model-only-for-test-chatgpt-auth-tokens";
    let codex_home = tempdir().expect("temp dir");
    let endpoint = TestModelsEndpoint::new(vec![vec![remote_model(
        dynamic_slug,
        "ChatGPT Auth Tokens",
        /*priority*/ 1,
    )]]);
    let auth = chatgpt_auth_tokens_for_tests(codex_home.path()).await;
    let manager = openai_manager_for_tests_with_auth(
        codex_home.path().to_path_buf(),
        endpoint.clone(),
        Some(AuthManager::from_auth_for_testing(auth)),
    );

    manager
        .refresh_available_models(RefreshStrategy::Online, &DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("refresh should fetch with ChatGPT auth tokens");

    assert!(
        manager
            .get_remote_models()
            .await
            .iter()
            .any(|candidate| candidate.slug == dynamic_slug),
        "remote refresh should include models fetched with ChatGPT auth tokens"
    );
    assert_eq!(
        endpoint.fetch_count(),
        1,
        "endpoint should fetch models with ChatGPT auth tokens"
    );
}

#[test]
fn build_available_models_picks_default_after_hiding_hidden_models() {
    let manager = static_manager_for_tests(ModelsResponse { models: Vec::new() });

    let hidden_model =
        remote_model_with_visibility("hidden", "Hidden", /*priority*/ 0, "hide");
    let visible_model =
        remote_model_with_visibility("visible", "Visible", /*priority*/ 1, "list");

    let expected_hidden = ModelPreset::from(hidden_model.clone());
    let mut expected_visible = ModelPreset::from(visible_model.clone());
    expected_visible.is_default = true;

    let available = manager.build_available_models(vec![hidden_model, visible_model]);

    assert_eq!(available, vec![expected_hidden, expected_visible]);
}

#[test]
fn build_available_models_filters_unsupported_model_info_before_projection() {
    let mut chatgpt_only = remote_model("chatgpt-only", "ChatGPT Only", 0);
    chatgpt_only.supported_in_api = false;
    let api_model = remote_model("api-model", "API Model", 1);
    let models = build_available_models_for_auth(
        vec![chatgpt_only, api_model],
        /*uses_codex_backend*/ false,
    );

    assert_eq!(
        models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>(),
        vec!["api-model"]
    );
}

#[tokio::test]
async fn static_manager_reads_latest_auth_mode() {
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let chatgpt_only_model = {
        let mut model = remote_model("chatgpt-only", "ChatGPT Only", /*priority*/ 0);
        model.supported_in_api = false;
        model
    };
    let api_model = remote_model("api-model", "API Model", /*priority*/ 1);
    let manager = StaticModelsManager::new(
        Some(Arc::clone(&auth_manager)),
        ModelsResponse {
            models: vec![chatgpt_only_model, api_model],
        },
    );

    let chatgpt_models = manager
        .list_models(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("list models");
    assert_eq!(
        chatgpt_models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>(),
        vec!["chatgpt-only", "api-model"]
    );

    auth_manager
        .set_external_auth(Arc::new(TestExternalApiKeyAuth))
        .await
        .expect("external API key auth should resolve");
    let api_models = manager
        .list_models(RefreshStrategy::Online, DEFAULT_HTTP_CLIENT_FACTORY)
        .await
        .expect("list models");

    assert_eq!(
        api_models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>(),
        vec!["api-model"]
    );
}

#[test]
fn bundled_models_json_roundtrips() {
    let response = crate::bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));

    let serialized =
        serde_json::to_string(&response).expect("bundled models.json should serialize");
    let roundtripped: ModelsResponse =
        serde_json::from_str(&serialized).expect("serialized models.json should deserialize");

    assert_eq!(
        response, roundtripped,
        "bundled models.json should round trip through serde"
    );
    assert!(
        !response.models.is_empty(),
        "bundled models.json should contain at least one model"
    );
}

#[test]
fn bundled_api_models_advertise_none_reasoning_effort() {
    let response = crate::bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));

    for slug in [
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.2",
    ] {
        let model = response
            .models
            .iter()
            .find(|model| model.slug == slug)
            .unwrap_or_else(|| panic!("bundled models.json should contain {slug}"));

        assert!(
            model
                .supported_reasoning_levels
                .iter()
                .any(|preset| preset.effort == ReasoningEffort::None),
            "{slug} should advertise the provider-supported none reasoning effort"
        );
    }
}
