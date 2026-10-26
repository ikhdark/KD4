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
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthEnvTelemetry;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::collect_auth_env_telemetry;
use codex_login::default_client::create_client_pool_with_headers;
use codex_login::default_client::default_headers;
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
    cache_identity_components: std::sync::OnceLock<crate::provider::ModelProviderCacheIdentity>,
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
            cache_identity_components: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn current_cache_identity(&self) -> String {
        self.cache_identity_for_auth_identity(crate::auth::provider_cache_auth_identity(
            self.auth_manager.as_deref(),
        ))
    }

    fn cache_identity_for_auth_identity(
        &self,
        auth_identity: crate::auth::ProviderCacheAuthIdentity,
    ) -> String {
        self.cache_identity_components
            .get_or_init(|| crate::provider::ModelProviderCacheIdentity::new(
                &self.model_provider_id,
                &self.provider_info,
            ))
            .for_auth_identity(&self.provider_info, auth_identity)
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
        let request_identity = self.cache_identity_for_auth_identity(
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
    pool: Mutex<Option<(HttpClientFactory, HeaderMap, bool, RouteAwareClientPool)>>,
}

impl RouteAwareModelsTransportBuilder {
    fn pool_for(&self, factory: HttpClientFactory) -> RouteAwareClientPool {
        self.pool_for_defaults(
            factory,
            default_headers(),
            std::env::var("CODEX_SANDBOX").as_deref() == Ok("seatbelt"),
        )
    }

    fn pool_for_defaults(
        &self,
        factory: HttpClientFactory,
        headers: HeaderMap,
        sandboxed: bool,
    ) -> RouteAwareClientPool {
        let mut cached = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((previous, previous_headers, previous_sandboxed, pool)) = cached.as_ref()
            && previous == &factory
            && previous_headers == &headers
            && *previous_sandboxed == sandboxed
        {
            return pool.clone();
        }
        let pool = create_client_pool_with_headers(
            factory.clone(),
            ClientRouteClass::Api,
            headers.clone(),
        );
        let pool = if sandboxed {
            pool.with_direct_routing()
        } else {
            pool
        };
        *cached = Some((factory, headers, sandboxed, pool.clone()));
        pool
    }
}

impl ModelsTransportBuilder for RouteAwareModelsTransportBuilder {
    fn build(
        &self,
        http_client_factory: HttpClientFactory,
        _request_url: String,
    ) -> ModelsTransportFuture<'_> {
        Box::pin(async move {
            Ok(ReqwestTransport::from_client_pool(
                self.pool_for(http_client_factory),
            ))
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

