use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_api::AgentIdentityTelemetry;
use codex_api::ModelsClient;
use codex_api::ModelsListResult;
use codex_api::RequestTelemetry;
use codex_api::ReqwestTransport;
use codex_api::TransportError;
use codex_api::auth_header_telemetry;
use codex_api::map_api_error;
use codex_feedback::FeedbackRequestTags;
use codex_feedback::emit_feedback_request_tags_with_auth_env;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_login::AuthEnvTelemetry;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::collect_auth_env_telemetry;
use codex_login::default_client::create_client_pool;
use codex_http_client::RouteAwareClientPool;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::ModelsFetchResult;
use codex_otel::TelemetryAuthMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ModelInfo;
use codex_response_debug_context::extract_response_debug_context;
use codex_response_debug_context::telemetry_transport_error_message;
use http::HeaderMap;
use http::header::IF_NONE_MATCH;
use tokio::time::timeout;

use crate::auth::agent_identity_telemetry;
use crate::auth::resolve_provider_auth;

// Command-backed auth retains its separately configured deadline. This bounds transport setup and HTTP only.
const MODELS_NETWORK_TIMEOUT: Duration = Duration::from_secs(5);
const MODELS_ENDPOINT: &str = "/models";

/// Provider-owned OpenAI-compatible `/models` endpoint.
#[derive(Debug)]
pub(crate) struct OpenAiModelsEndpoint {
    model_provider_id: String,
    provider_info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
    transport_builder: Arc<dyn ModelsTransportBuilder>,
}


impl OpenAiModelsEndpoint {
    pub(crate) fn new(
        model_provider_id: String,
        provider_info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self {
            model_provider_id,
            provider_info,
            auth_manager,
            transport_builder: Arc::new(RouteAwareModelsTransportBuilder::default()),
        }
    }

    async fn auth(&self) -> Option<CodexAuth> {
        match self.auth_manager.as_ref() {
            Some(auth_manager) => auth_manager.auth().await,
            None => None,
        }
    }

    async fn uses_codex_backend(&self) -> bool {
        self.auth()
            .await
            .as_ref()
            .is_some_and(CodexAuth::uses_codex_backend)
    }

    async fn list_models(
        &self,
        client_version: &str,
        http_client_factory: HttpClientFactory,
    ) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        match self
            .list_models_conditional(client_version, http_client_factory, None)
            .await?
        {
            ModelsFetchResult::Modified { models, etag } => Ok((models, etag)),
            ModelsFetchResult::NotModified => Err(CodexErr::InvalidRequest(
                "models endpoint returned 304 without an ETag validator".to_string(),
            )),
        }
    }

    async fn list_models_conditional(
        &self,
        client_version: &str,
        http_client_factory: HttpClientFactory,
        etag: Option<&str>,
    ) -> CoreResult<ModelsFetchResult> {
        self.list_models_for_identity(client_version, http_client_factory, etag, None)
            .await
    }

    async fn list_models_for_identity(
        &self,
        client_version: &str,
        http_client_factory: HttpClientFactory,
        etag: Option<&str>,
        expected_identity: Option<&str>,
    ) -> CoreResult<ModelsFetchResult> {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.fetch_update.duration_ms", &[]);
        let mut headers = HeaderMap::new();
        if let Some(etag) = etag {
            headers.insert(
                IF_NONE_MATCH,
                etag.parse().map_err(|err| {
                    CodexErr::InvalidRequest(format!("invalid models ETag validator: {err}"))
                })?,
            );
        }
        let auth = self.auth().await;
        let request_identity = crate::provider::model_provider_cache_identity_for_auth_identity(
            &self.model_provider_id,
            &self.provider_info,
            crate::auth::provider_cache_auth_identity_for_auth(
                auth.as_ref(),
                self.auth_manager.as_deref(),
            ),
        );
        if expected_identity.is_some_and(|expected| expected != request_identity) {
            return Err(CodexErr::InvalidRequest(
                "model catalog authentication changed before dispatch".into(),
            ));
        }
        let auth_mode = auth.as_ref().map(CodexAuth::auth_mode);
        let api_provider = crate::provider::request_api_provider(&self.provider_info, auth_mode)?;
        let api_auth = resolve_provider_auth(auth.as_ref(), &self.provider_info)?;
        let request_url =
            ModelsClient::<ReqwestTransport>::request_url(&api_provider, client_version);
        let auth_telemetry = auth_header_telemetry(api_auth.as_ref());
        let agent_identity_telemetry = if let Some(CodexAuth::AgentIdentity(auth)) = auth.as_ref() {
            Some(agent_identity_telemetry(auth))
        } else {
            None
        };
        let request_telemetry: Arc<dyn RequestTelemetry> = Arc::new(ModelsRequestTelemetry {
            auth_mode: auth_mode.map(|mode| TelemetryAuthMode::from(mode).to_string()),
            auth_header_attached: auth_telemetry.attached,
            auth_header_name: auth_telemetry.name,
            agent_identity_telemetry,
            auth_env: self.auth_env(),
        });
        timeout(MODELS_NETWORK_TIMEOUT, async {
            let transport = self
                .transport_for(http_client_factory, request_url.clone())
                .await?;
            let client = ModelsClient::new(transport, api_provider, api_auth)
                .with_telemetry(Some(request_telemetry));
            client
                .list_models_conditional(request_url, headers)
                .await
                .map_err(map_api_error)
                .and_then(|result| match result {
                    ModelsListResult::Modified { models, etag } => {
                        Ok(ModelsFetchResult::Modified { models, etag })
                    }
                    ModelsListResult::NotModified if etag.is_some() => {
                        Ok(ModelsFetchResult::NotModified)
                    }
                    ModelsListResult::NotModified => Err(CodexErr::InvalidRequest(
                        "models endpoint returned 304 without an ETag validator".to_string(),
                    )),
                })
        })
        .await
        .map_err(|_| CodexErr::Timeout)?
    }

    async fn transport_for(
        &self,
        http_client_factory: HttpClientFactory,
        request_url: String,
    ) -> std::io::Result<ReqwestTransport> {
        self.transport_builder
            .build(http_client_factory, request_url)
            .await
    }

    fn auth_env(&self) -> AuthEnvTelemetry {
        let codex_api_key_env_enabled = self
            .auth_manager
            .as_ref()
            .is_some_and(|auth_manager| auth_manager.codex_api_key_env_enabled());
        collect_auth_env_telemetry(&self.provider_info, codex_api_key_env_enabled)
    }
}

impl ModelsEndpointClient for OpenAiModelsEndpoint {
    fn has_command_auth(&self) -> bool {
        self.provider_info.has_command_auth()
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(OpenAiModelsEndpoint::uses_codex_backend(self))
    }

    fn list_models<'a>(
        &'a self,
        client_version: &'a str,
        http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>> {
        Box::pin(OpenAiModelsEndpoint::list_models(
            self,
            client_version,
            http_client_factory,
        ))
    }

    fn list_models_conditional<'a>(
        &'a self,
        client_version: &'a str,
        http_client_factory: HttpClientFactory,
        etag: Option<&'a str>,
    ) -> ModelsEndpointFuture<'a, CoreResult<ModelsFetchResult>> {
        Box::pin(OpenAiModelsEndpoint::list_models_conditional(
            self,
            client_version,
            http_client_factory,
            etag,
        ))
    }
    fn list_models_for_identity<'a>(
        &'a self,
        client_version: &'a str,
        http_client_factory: HttpClientFactory,
        etag: Option<&'a str>,
        expected_identity: &'a str,
    ) -> ModelsEndpointFuture<'a, CoreResult<ModelsFetchResult>> {
        Box::pin(OpenAiModelsEndpoint::list_models_for_identity(
            self,
            client_version,
            http_client_factory,
            etag,
            Some(expected_identity),
        ))
    }
}

type ModelsTransportFuture<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<ReqwestTransport>> + Send + 'a>>;

/// Builds the concrete transport selected for one models request.
///
/// Implementations must honor the supplied request-time client factory and exact request URL.
trait ModelsTransportBuilder: fmt::Debug + Send + Sync {
    fn build(
        &self,
        http_client_factory: HttpClientFactory,
        request_url: String,
    ) -> ModelsTransportFuture<'_>;
}

#[derive(Debug, Default)]
struct RouteAwareModelsTransportBuilder {
    pool: Mutex<Option<(HttpClientFactory, RouteAwareClientPool)>>,
    sandbox_transport: tokio::sync::OnceCell<ReqwestTransport>,
}

impl RouteAwareModelsTransportBuilder {
    fn pool_for(&self, factory: HttpClientFactory) -> RouteAwareClientPool {
        let mut cached = self.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((previous, pool)) = cached.as_ref()
            && previous == &factory
        {
            return pool.clone();
        }
        let pool = create_client_pool(factory.clone(), ClientRouteClass::Api);
        *cached = Some((factory, pool.clone()));
        pool
    }
}

impl ModelsTransportBuilder for RouteAwareModelsTransportBuilder {
    fn build(
        &self,
        http_client_factory: HttpClientFactory,
        request_url: String,
    ) -> ModelsTransportFuture<'_> {
        Box::pin(async move {
            if std::env::var("CODEX_SANDBOX").as_deref() == Ok("seatbelt") {
                // Preserve the default client's sandbox-specific direct routing.
                return self.sandbox_transport.get_or_try_init(|| async {
                    codex_login::default_client::create_client_for_route_async(
                        http_client_factory, request_url, ClientRouteClass::Api,
                    ).await.map(ReqwestTransport::from_http_client)
                }).await.cloned();
            }
            Ok(ReqwestTransport::from_client_pool(self.pool_for(http_client_factory)))
        })
    }
}

#[derive(Clone)]
struct ModelsRequestTelemetry {
    auth_mode: Option<String>,
    auth_header_attached: bool,
    auth_header_name: Option<&'static str>,
    agent_identity_telemetry: Option<AgentIdentityTelemetry>,
    auth_env: AuthEnvTelemetry,
}

impl RequestTelemetry for ModelsRequestTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<http::StatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        let success = status
            .is_some_and(|code| code.is_success() || code == http::StatusCode::NOT_MODIFIED)
            && error.is_none();
        let error_message = error.map(telemetry_transport_error_message);
        let response_debug = error
            .map(extract_response_debug_context)
            .unwrap_or_default();
        let status = status.map(|status| status.as_u16());
        tracing::event!(
            target: "codex_otel.log_only",
            tracing::Level::INFO,
            event.name = "codex.api_request",
            duration_ms = %duration.as_millis(),
            http.response.status_code = status,
            success = success,
            error.message = error_message.as_deref(),
            attempt = attempt,
            endpoint = MODELS_ENDPOINT,
            auth.header_attached = self.auth_header_attached,
            auth.header_name = self.auth_header_name,
            auth.env_openai_api_key_present = self.auth_env.openai_api_key_env_present,
            auth.env_codex_api_key_present = self.auth_env.codex_api_key_env_present,
            auth.env_codex_api_key_enabled = self.auth_env.codex_api_key_env_enabled,
            auth.env_provider_key_name = self.auth_env.provider_env_key_name.as_deref(),
            auth.env_provider_key_present = self.auth_env.provider_env_key_present,
            auth.env_refresh_token_url_override_present = self.auth_env.refresh_token_url_override_present,
            auth.request_id = response_debug.request_id.as_deref(),
            auth.cf_ray = response_debug.cf_ray.as_deref(),
            auth.error = response_debug.auth_error.as_deref(),
            auth.error_code = response_debug.auth_error_code.as_deref(),
            auth.mode = self.auth_mode.as_deref(),
            auth.agent_id = self.agent_identity_telemetry.as_ref().map(|metadata| metadata.agent_id.as_str()),
            auth.task_id = self.agent_identity_telemetry.as_ref().map(|metadata| metadata.task_id.as_str()),
        );
        tracing::event!(
            target: "codex_otel.trace_safe",
            tracing::Level::INFO,
            event.name = "codex.api_request",
            duration_ms = %duration.as_millis(),
            http.response.status_code = status,
            success = success,
            error.message = error_message.as_deref(),
            attempt = attempt,
            endpoint = MODELS_ENDPOINT,
            auth.header_attached = self.auth_header_attached,
            auth.header_name = self.auth_header_name,
            auth.env_openai_api_key_present = self.auth_env.openai_api_key_env_present,
            auth.env_codex_api_key_present = self.auth_env.codex_api_key_env_present,
            auth.env_codex_api_key_enabled = self.auth_env.codex_api_key_env_enabled,
            auth.env_provider_key_name = self.auth_env.provider_env_key_name.as_deref(),
            auth.env_provider_key_present = self.auth_env.provider_env_key_present,
            auth.env_refresh_token_url_override_present = self.auth_env.refresh_token_url_override_present,
            auth.request_id = response_debug.request_id.as_deref(),
            auth.cf_ray = response_debug.cf_ray.as_deref(),
            auth.error = response_debug.auth_error.as_deref(),
            auth.error_code = response_debug.auth_error_code.as_deref(),
            auth.mode = self.auth_mode.as_deref(),
            auth.agent_id = self.agent_identity_telemetry.as_ref().map(|metadata| metadata.agent_id.as_str()),
            auth.task_id = self.agent_identity_telemetry.as_ref().map(|metadata| metadata.task_id.as_str()),
        );
        emit_feedback_request_tags_with_auth_env(
            &FeedbackRequestTags {
                endpoint: MODELS_ENDPOINT,
                auth_header_attached: self.auth_header_attached,
                auth_header_name: self.auth_header_name,
                auth_mode: self.auth_mode.as_deref(),
                auth_retry_after_unauthorized: None,
                auth_recovery_mode: None,
                auth_recovery_phase: None,
                auth_connection_reused: None,
                auth_request_id: response_debug.request_id.as_deref(),
                auth_cf_ray: response_debug.cf_ray.as_deref(),
                auth_error: response_debug.auth_error.as_deref(),
                auth_error_code: response_debug.auth_error_code.as_deref(),
                auth_recovery_followup_success: None,
                auth_recovery_followup_status: None,
            },
            &self.auth_env,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;
    use codex_http_client::OutboundProxyPolicy;
    use codex_protocol::config_types::ModelProviderAuthInfo;
    use codex_protocol::openai_models::ModelsResponse;
    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::matchers::query_param;

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[tokio::test]
    async fn catalog_refresh_observes_changed_proxy_and_reuses_route_clients() {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        for server in [&first, &second] {
            Mock::given(path("/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ModelsResponse { models: Vec::new() }))
                .expect(2)
                .mount(server).await;
        }
        let base = format!("http://catalog-{}.invalid", first.address().port());
        let url = format!("{base}/models?client_version=route-test");
        let factory = HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy);
        let builder = Arc::new(RouteAwareModelsTransportBuilder::default());
        let endpoint = OpenAiModelsEndpoint {
            model_provider_id: "test".into(),
            provider_info: ModelProviderInfo::create_openai_provider(Some(base)),
            auth_manager: None,
            transport_builder: builder.clone(),
        };
        for server in [&first, &first, &second, &second] {
            codex_http_client::cache_system_proxy_route_for_test(&url, server.uri());
            endpoint.list_models("route-test", factory.clone()).await.unwrap();
        }
        assert_eq!(builder.pool_for(factory).cached_route_count(), 2);
    }

    #[derive(Debug)]
    struct RecordingTransportBuilder {
        observed_request: Arc<Mutex<Option<(OutboundProxyPolicy, String)>>>,
        build_count: Arc<AtomicUsize>,
        inner: RouteAwareModelsTransportBuilder,
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[tokio::test]
    async fn catalog_redirect_resolves_the_new_destination_route() {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        let base = format!("http://catalog-redirect-{}.invalid", first.address().port());
        let destination = format!("http://catalog-target-{}.invalid/next", second.address().port());
        Mock::given(path("/models"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", destination.as_str()))
            .expect(1).mount(&first).await;
        Mock::given(path("/next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ModelsResponse { models: Vec::new() }))
            .expect(1).mount(&second).await;
        codex_http_client::cache_system_proxy_route_for_test(
            &format!("{base}/models?client_version=redirect-test"), first.uri(),
        );
        codex_http_client::cache_system_proxy_route_for_test(&destination, second.uri());
        OpenAiModelsEndpoint::new(
            "test".into(), ModelProviderInfo::create_openai_provider(Some(base)), None,
        ).list_models(
            "redirect-test", HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
        ).await.unwrap();
    }

    impl ModelsTransportBuilder for RecordingTransportBuilder {
        fn build(
            &self,
            http_client_factory: HttpClientFactory,
            request_url: String,
        ) -> ModelsTransportFuture<'_> {
            let observed_request = Arc::clone(&self.observed_request);
            let build_count = Arc::clone(&self.build_count);
            Box::pin(async move {
                let pool = self.inner.pool_for(http_client_factory.clone());
                if pool.cached_route_count() == 0 {
                    build_count.fetch_add(1, Ordering::SeqCst);
                }
                *observed_request
                    .lock()
                    .expect("observed request lock should not be poisoned") =
                    Some((http_client_factory.outbound_proxy_policy(), request_url.clone()));
                Ok(ReqwestTransport::from_http_client(
                    pool.client_for_url(&request_url).await.map_err(std::io::Error::other)?,
                ))
            })
        }
    }

    fn provider_info_with_command_auth() -> ModelProviderInfo {
        ModelProviderInfo {
            auth: Some(ModelProviderAuthInfo {
                command: "print-token".to_string(),
                args: Vec::new(),
                timeout_ms: NonZeroU64::new(5_000).expect("timeout should be non-zero"),
                refresh_interval_ms: 300_000,
                cwd: std::env::current_dir()
                    .expect("current dir should be available")
                    .try_into()
                    .expect("current dir should be absolute"),
            }),
            requires_openai_auth: false,
            ..ModelProviderInfo::create_openai_provider(/*base_url*/ None)
        }
    }

    #[tokio::test]
    async fn audit_catalog_request_rejects_mismatched_frozen_auth_before_dispatch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        let provider = ModelProviderInfo::create_openai_provider(Some(server.uri()));
        let endpoint = OpenAiModelsEndpoint::new("test".into(), provider.clone(), None);
        let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
        let result = endpoint
            .list_models_for_identity(
                "0.0.0",
                factory.clone(),
                Some("etag-a"),
                Some("wrong-scope"),
            )
            .await;
        assert!(matches!(result, Err(CodexErr::InvalidRequest(_))));
        assert!(server.received_requests().await.unwrap().is_empty());
        let identity = crate::provider::model_provider_cache_identity_for_auth_identity(
            "test",
            &provider,
            crate::auth::provider_cache_auth_identity_for_auth(None, None),
        );
        assert!(matches!(
            endpoint
                .list_models_for_identity("0.0.0", factory, Some("etag-a"), Some(&identity))
                .await
                .unwrap(),
            ModelsFetchResult::NotModified
        ));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].headers[IF_NONE_MATCH], "etag-a");
    }

    #[test]
    fn command_auth_provider_reports_command_auth_without_cached_auth() {
        let endpoint = OpenAiModelsEndpoint::new(
            "test".into(),
            provider_info_with_command_auth(),
            /*auth_manager*/ None,
        );

        assert!(endpoint.has_command_auth());
    }

    #[test]
    fn provider_without_command_auth_reports_no_command_auth() {
        let endpoint = OpenAiModelsEndpoint::new(
            "test".into(),
            ModelProviderInfo::create_openai_provider(/*base_url*/ None),
            /*auth_manager*/ None,
        );

        assert!(!endpoint.has_command_auth());
    }

    #[tokio::test]
    async fn model_request_uses_request_time_proxy_policy_and_exact_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(query_param("client_version", "0.0.0"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ModelsResponse { models: Vec::new() }),
            )
            .expect(1)
            .mount(&server)
            .await;

        let observed_request = Arc::new(Mutex::new(None));
        let endpoint = OpenAiModelsEndpoint {
            model_provider_id: "test".into(),
            provider_info: ModelProviderInfo::create_openai_provider(Some(server.uri())),
            auth_manager: None,
            transport_builder: Arc::new(RecordingTransportBuilder {
                inner: RouteAwareModelsTransportBuilder::default(),
                observed_request: Arc::clone(&observed_request),
                build_count: Arc::new(AtomicUsize::new(0)),
            }),
        };

        endpoint
            .list_models(
                "0.0.0",
                HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
            )
            .await
            .expect("models request should succeed");

        assert_eq!(
            *observed_request
                .lock()
                .expect("observed request lock should not be poisoned"),
            Some((
                OutboundProxyPolicy::RespectSystemProxy,
                format!("{}/models?client_version=0.0.0", server.uri()),
            ))
        );
    }

    #[tokio::test]
    async fn model_requests_reuse_transport_for_same_factory_and_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(query_param("client_version", "0.0.0"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ModelsResponse { models: Vec::new() }),
            )
            .expect(2)
            .mount(&server)
            .await;

        let build_count = Arc::new(AtomicUsize::new(0));
        let endpoint = OpenAiModelsEndpoint {
            model_provider_id: "test".into(),
            provider_info: ModelProviderInfo::create_openai_provider(Some(server.uri())),
            auth_manager: None,
            transport_builder: Arc::new(RecordingTransportBuilder {
                inner: RouteAwareModelsTransportBuilder::default(),
                observed_request: Arc::new(Mutex::new(None)),
                build_count: Arc::clone(&build_count),
            }),
        };
        let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);

        endpoint
            .list_models("0.0.0", factory.clone())
            .await
            .expect("first models request should succeed");
        endpoint
            .list_models("0.0.0", factory)
            .await
            .expect("second models request should succeed");

        assert_eq!(build_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn model_transport_pool_is_keyed_by_factory_and_resolves_exact_url() {
        let build_count = Arc::new(AtomicUsize::new(0));
        let observed_request = Arc::new(Mutex::new(None));
        let endpoint = OpenAiModelsEndpoint {
            model_provider_id: "test".into(),
            provider_info: ModelProviderInfo::create_openai_provider(/*base_url*/ None),
            auth_manager: None,
            transport_builder: Arc::new(RecordingTransportBuilder {
                inner: RouteAwareModelsTransportBuilder::default(),
                observed_request: Arc::clone(&observed_request),
                build_count: Arc::clone(&build_count),
            }),
        };
        let default_factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);

        endpoint
            .transport_for(
                default_factory.clone(),
                "https://example.com/models?a=1".to_string(),
            )
            .await
            .expect("first transport should build");
        endpoint
            .transport_for(
                default_factory.clone(),
                "https://example.com/models?a=1".to_string(),
            )
            .await
            .expect("identical route should reuse the transport");
        endpoint
            .transport_for(
                default_factory.clone(),
                "https://example.com/models?a=2".to_string(),
            )
            .await
            .expect("a new URL with the same route should reuse the client");
        assert_eq!(build_count.load(Ordering::SeqCst), 1);
        endpoint
            .transport_for(
                HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
                "https://example.com/models?a=1".to_string(),
            )
            .await
            .expect("changed policy should rebuild the transport");
        assert_eq!(
            *observed_request
                .lock()
                .expect("observed request lock should not be poisoned"),
            Some((
                OutboundProxyPolicy::RespectSystemProxy,
                "https://example.com/models?a=1".to_string(),
            ))
        );
        endpoint
            .transport_for(
                default_factory,
                "https://example.com/models?a=2".to_string(),
            )
            .await
            .expect("changed factory should rebuild the pool");

        assert_eq!(build_count.load(Ordering::SeqCst), 3);
        assert_eq!(
            *observed_request
                .lock()
                .expect("observed request lock should not be poisoned"),
            Some((
                OutboundProxyPolicy::ReqwestDefault,
                "https://example.com/models?a=2".to_string(),
            ))
        );
    }

    #[tokio::test]
    async fn conditional_model_request_sends_etag_and_accepts_not_modified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(query_param("client_version", "0.0.0"))
            .and(header("if-none-match", "\"models-etag\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let endpoint = OpenAiModelsEndpoint {
            model_provider_id: "test".into(),
            provider_info: ModelProviderInfo::create_openai_provider(Some(server.uri())),
            auth_manager: None,
            transport_builder: Arc::new(RouteAwareModelsTransportBuilder::default()),
        };

        let result = endpoint
            .list_models_conditional(
                "0.0.0",
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
                Some("\"models-etag\""),
            )
            .await
            .expect("conditional models request should succeed");

        assert!(matches!(result, ModelsFetchResult::NotModified));
    }
    #[tokio::test]
    async fn conditional_models_rejects_unvalidated_not_modified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        let endpoint = OpenAiModelsEndpoint::new(
            "test".into(),
            ModelProviderInfo::create_openai_provider(Some(server.uri())),
            None,
        );
        let result = endpoint
            .list_models_conditional(
                "0.0.0",
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
                None,
            )
            .await;
        assert!(
            matches!(result, Err(CodexErr::UnexpectedStatus(response)) if response.status == http::StatusCode::NOT_MODIFIED)
        );
    }

    #[tokio::test]
    async fn invalid_etag_does_not_build_transport() {
        let build_count = Arc::new(AtomicUsize::new(0));
        let endpoint = OpenAiModelsEndpoint {
            model_provider_id: "test".into(),
            provider_info: ModelProviderInfo::create_openai_provider(None),
            auth_manager: None,
            transport_builder: Arc::new(RecordingTransportBuilder {
                inner: RouteAwareModelsTransportBuilder::default(),
                observed_request: Arc::new(Mutex::new(None)),
                build_count: build_count.clone(),
            }),
        };
        let result = endpoint
            .list_models_conditional(
                "0.0.0",
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
                Some("invalid\nheader"),
            )
            .await;
        assert!(
            matches!(result, Err(CodexErr::InvalidRequest(message)) if message.contains("invalid models ETag"))
        );
        assert_eq!(build_count.load(Ordering::SeqCst), 0);
    }
}
