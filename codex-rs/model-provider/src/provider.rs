use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use codex_api::ApiError;
use codex_api::Provider;
use codex_api::SharedAuthProvider;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::ModelsCacheIdentity;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::account::ProviderAccount;
use codex_protocol::auth::AuthMode;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ModelsResponse;
use sha2::Digest;
use sha2::Sha256;

use crate::amazon_bedrock::AmazonBedrockModelProvider;
use crate::auth::ProviderAuthScope;
use crate::auth::ResolvedProviderAuth;
use crate::auth::auth_manager_for_provider;
use crate::auth::resolve_provider_auth;
use crate::auth::resolve_provider_auth_for_scope;
use crate::models_endpoint::OpenAiModelsEndpoint;

/// Optional provider-backed features that Codex may expose at runtime.
///
/// These capabilities are a provider-owned upper bound. Callers can disable
/// more functionality through normal config, but should not expose a feature
/// that the active provider marks unsupported here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub namespace_tools: bool,
    pub image_generation: bool,
    pub web_search: bool,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            namespace_tools: true,
            image_generation: true,
            web_search: true,
        }
    }
}

/// Current app-visible account state for a model provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountState {
    pub account: Option<ProviderAccount>,
    pub requires_openai_auth: bool,
}

/// Provider configuration and credentials resolved from one request-time auth view.
pub struct ResolvedModelProviderClientSetup {
    pub auth: Option<CodexAuth>,
    pub api_provider: Provider,
    pub resolved_auth: ResolvedProviderAuth,
}

/// Error returned when a provider cannot construct its app-visible account state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAccountError {
    CredentialsUnavailable(std::io::ErrorKind),
    MissingChatgptAccountDetails,
    UnsupportedBedrockApiKeyAuth,
}

impl fmt::Display for ProviderAccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CredentialsUnavailable(kind) => write!(
                f,
                "credentials unavailable ({kind:?}); reload authentication to retry"
            ),
            Self::MissingChatgptAccountDetails => {
                write!(f, "plan type is required for chatgpt authentication")
            }
            Self::UnsupportedBedrockApiKeyAuth => {
                write!(
                    f,
                    "Bedrock API key auth is only supported by the Amazon Bedrock model provider"
                )
            }
        }
    }
}

impl std::error::Error for ProviderAccountError {}

pub type ProviderAccountResult = std::result::Result<ProviderAccountState, ProviderAccountError>;

/// Runtime provider abstraction used by model execution.
///
/// Implementations own provider-specific behavior for a model backend. The
/// `ModelProviderInfo` returned by `info` is the serialized/configured provider
/// metadata used by the default OpenAI-compatible implementation.
pub trait ModelProvider: fmt::Debug + Send + Sync {
    /// Returns the configured provider metadata.
    fn info(&self) -> &ModelProviderInfo;

    /// Returns the provider-owned capability upper bounds.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Intersect provider upper bounds with model- and region-specific eligibility.
    fn supports_hosted_web_search(&self, _model: &str) -> bool {
        self.capabilities().web_search
    }

    /// Resolve provider-owned metadata needed by the synchronous tool planner.
    fn prepare_hosted_web_search(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Whether hosted search must remain inside the provider's index/cache boundary.
    fn hosted_web_search_requires_cached_access(&self) -> bool {
        false
    }

    /// Returns whether requests made through this provider should include attestation.
    fn supports_attestation(&self) -> bool {
        false
    }

    /// Returns the provider-scoped auth manager, when this provider uses one.
    ///
    /// TODO(celia-oai): Make auth manager access internal to this crate so callers
    /// resolve provider-specific auth only through `ModelProvider`. We first need
    /// to think through whether Codex should have a unified provider-specific auth
    /// manager throughout the codebase; that is a larger refactor than this change.
    fn auth_manager(&self) -> Option<Arc<AuthManager>>;

    /// Returns whether this provider requires first-party credentials that are
    /// not currently cached, as during a logout or account switch. A request
    /// dispatched now would carry no credentials.
    fn missing_required_credentials(&self) -> bool {
        false
    }

    /// Returns the current provider-scoped auth value, if one is configured.
    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>>;

    /// Returns the current app-visible account state for this provider.
    fn account_state(&self) -> ProviderAccountResult;

    /// Maps an API client error into the provider's user-facing error representation.
    fn map_api_error(&self, error: ApiError) -> CodexErr {
        codex_api::map_api_error(error)
    }

    /// Returns provider configuration adapted for the API client.
    fn api_provider(&self) -> ModelProviderFuture<'_, codex_protocol::error::Result<Provider>> {
        Box::pin(async move {
            let auth = self.auth().await;
            request_api_provider(self.info(), auth.as_ref().map(CodexAuth::auth_mode))
        })
    }

    /// Returns the provider base URL that will be used at request time.
    fn runtime_base_url(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<Option<String>>> {
        Box::pin(async { Ok(self.info().base_url.clone()) })
    }

    /// Returns the auth provider used to attach request credentials.
    fn api_auth(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<SharedAuthProvider>> {
        Box::pin(async move {
            let auth = self.auth().await;
            resolve_provider_auth(auth.as_ref(), self.info())
        })
    }

    /// Returns request credentials, optionally scoped to a Codex session task.
    /// Unscoped extension setup. Custom providers retain their accessor overrides;
    /// configured providers override this to resolve credentials exactly once.
    fn resolve_unscoped_client_setup(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<(Provider, SharedAuthProvider)>>
    {
        Box::pin(async move { Ok((self.api_provider().await?, self.api_auth().await?)) })
    }

    /// Resolves a Codex-backend-only request from one auth snapshot.
    /// Custom providers must explicitly implement this binding; the default
    /// cannot prove that their independent accessors share an auth view.
    fn resolve_codex_backend_client_setup(
        &self,
    ) -> ModelProviderFuture<
        '_,
        codex_protocol::error::Result<Option<(Provider, SharedAuthProvider)>>,
    > {
        Box::pin(async { Ok(None) })
    }

    /// Returns request credentials, optionally scoped to a Codex session task.
    fn api_auth_for_scope(
        &self,
        scope: ProviderAuthScope,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<ResolvedProviderAuth>> {
        Box::pin(async move {
            if !provider_uses_first_party_auth_path(self.info()) {
                return self.api_auth().await.map(ResolvedProviderAuth::new);
            }
            let auth = self.auth().await;
            resolve_provider_auth_for_scope(self.auth_manager(), auth.as_ref(), self.info(), scope)
                .await
        })
    }

    /// Resolves all request setup that must agree on the same provider auth state.
    fn resolve_client_setup(
        &self,
        scope: ProviderAuthScope,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<ResolvedModelProviderClientSetup>>
    {
        Box::pin(async move {
            let auth = self.auth().await;
            let api_provider = self.api_provider().await?;
            let resolved_auth = self.api_auth_for_scope(scope).await?;
            Ok(ResolvedModelProviderClientSetup {
                auth,
                api_provider,
                resolved_auth,
            })
        })
    }

    /// Creates the model manager implementation appropriate for this provider.
    fn models_manager(
        &self,
        model_provider_id: &str,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager;
}

pub type ModelProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shared runtime model provider handle.
pub type SharedModelProvider = Arc<dyn ModelProvider>;

fn provider_uses_first_party_auth_path(provider: &ModelProviderInfo) -> bool {
    provider.requires_openai_auth
        && provider.env_key.is_none()
        && provider.experimental_bearer_token.is_none()
        && provider.auth.is_none()
        && provider.aws.is_none()
}

/// Converts provider metadata into the API provider used for requests.
///
/// The built-in OpenAI provider sends its package version in the `version`
/// header. Source builds would report 0.0.0, which the backend rejects for
/// models that discovery lists under the compatibility baseline, so advertise
/// that baseline in the header and in the matching User-Agent version. Other
/// `version` values are sent unchanged.
pub(crate) fn request_api_provider(
    info: &ModelProviderInfo,
    auth_mode: Option<AuthMode>,
) -> codex_protocol::error::Result<Provider> {
    let mut provider = info.to_api_provider(auth_mode)?;
    let Some(version) = provider
        .headers
        .get("version")
        .and_then(|value| value.to_str().ok())
        .and_then(codex_models_manager::source_build_client_version)
    else {
        return Ok(provider);
    };
    provider
        .headers
        .insert("version", http::HeaderValue::from_static(version));
    let originator = codex_login::default_client::originator().value;
    let user_agent = codex_login::default_client::get_codex_user_agent();
    if let Some(rest) =
        user_agent.strip_prefix(&format!("{originator}/{}", env!("CARGO_PKG_VERSION")))
        && let Ok(user_agent) =
            http::HeaderValue::from_str(&format!("{originator}/{version}{rest}"))
    {
        provider
            .headers
            .insert(http::header::USER_AGENT, user_agent);
    }
    Ok(provider)
}



/// Immutable configuration components only. Environment and account authority
/// are deliberately projected afresh for every complete identity.
pub(crate) struct ModelProviderCacheIdentity {
    configured: serde_json::Value,
    normalized_base_urls: BTreeMap<String, String>,
}

impl fmt::Debug for ModelProviderCacheIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelProviderCacheIdentity { .. }")
    }
}

impl ModelProviderCacheIdentity {
    pub(crate) fn new(model_provider_id: &str, provider_info: &ModelProviderInfo) -> Self {
        let query_parameters = provider_info
            .query_params
            .as_ref()
            .into_iter()
            .flat_map(|values| values.iter())
            .map(|(key, value)| (key.clone(), model_cache_secret_component(b"query-parameter", value)))
            .collect::<BTreeMap<_, _>>();
        let http_headers = provider_info
            .http_headers
            .as_ref()
            .into_iter()
            .flat_map(|values| values.iter())
            .map(|(key, value)| {
                (key.to_ascii_lowercase(), model_cache_secret_component(b"http-header", value))
            })
            .collect::<BTreeMap<_, _>>();
        let command_auth = provider_info.auth.as_ref().map(|auth| {
            serde_json::json!({
                "command": &auth.command,
                "args": &auth.args,
                "cwd": &auth.cwd,
            })
        });
        // Keep the exact existing object keys/order and V2 digest contract.
        // Dynamic placeholders are overlaid without inserting/removing keys.
        let configured = serde_json::json!({
            "version": 2,
            "provider_id": model_provider_id,
            "provider_name": &provider_info.name,
            "effective_base_url": null,
            "wire_api": provider_info.wire_api.to_string(),
            "query_parameters": query_parameters,
            "http_headers": http_headers,
            "env_http_headers": null,
            "env_key": null,
            "experimental_bearer_token_digest": provider_info.experimental_bearer_token.as_deref()
                .map(|value| model_cache_secret_component(b"experimental-bearer-token", value)),
            "command_auth": command_auth,
            "requires_openai_auth": provider_info.requires_openai_auth,
            "effective_auth_mode": null,
            "account_scope_digest": null,
            "aws": &provider_info.aws,
        });
        let normalized_base_urls = [None, Some(AuthMode::Chatgpt)]
            .into_iter()
            .map(|mode| {
                let raw = provider_info.effective_base_url(mode);
                (raw.to_owned(), normalize_model_cache_base_url(raw))
            })
            .collect();
        Self { configured, normalized_base_urls }
    }

    pub(crate) fn for_auth_identity(
        &self,
        provider_info: &ModelProviderInfo,
        auth_identity: crate::auth::ProviderCacheAuthIdentity,
    ) -> String {
        let raw_base_url = provider_info.effective_base_url(auth_identity.auth_mode);
        let effective_base_url = self.normalized_base_urls.get(raw_base_url).cloned()
            .unwrap_or_else(|| normalize_model_cache_base_url(raw_base_url));
        let env_http_headers = provider_info
            .env_http_headers
            .as_ref()
            .into_iter()
            .flat_map(|values| values.iter())
            .map(|(header, env_var)| {
                let value_digest = std::env::var(env_var)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| model_cache_secret_component(b"environment-header", &value));
                (header.to_ascii_lowercase(), serde_json::json!({
                    "env_var": env_var, "value_digest": value_digest,
                }))
            })
            .collect::<BTreeMap<_, _>>();
        let env_key = provider_info.env_key.as_ref().map(|env_var| {
            serde_json::json!({
                "env_var": env_var,
                "value_digest": std::env::var(env_var)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| model_cache_secret_component(b"environment-api-key", &value)),
            })
        });
        let dynamic = BTreeMap::from([
            ("effective_base_url", serde_json::json!(effective_base_url)),
            ("env_http_headers", serde_json::json!(env_http_headers)),
            ("env_key", serde_json::json!(env_key)),
            ("effective_auth_mode", serde_json::json!(auth_identity.auth_mode)),
            ("account_scope_digest", serde_json::json!(auth_identity.account_scope_digest)),
        ]);
        struct IdentityOverlay<'a> {
            configured: &'a serde_json::Value,
            dynamic: &'a BTreeMap<&'static str, serde_json::Value>,
        }
        impl serde::Serialize for IdentityOverlay<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let Some(configured) = self.configured.as_object() else {
                    return serde::Serialize::serialize(self.configured, serializer);
                };
                let mut map = serializer.serialize_map(Some(configured.len()))?;
                for (key, value) in configured {
                    map.serialize_entry(key, self.dynamic.get(key.as_str()).unwrap_or(value))?;
                }
                map.end()
            }
        }
        let value = IdentityOverlay { configured: &self.configured, dynamic: &dynamic };
        let canonical = serde_json::to_vec(&value).unwrap_or_else(|error| {
            let mut fallback = b"\0provider-cache-identity-serialization-error:".to_vec();
            fallback.extend_from_slice(error.to_string().as_bytes());
            fallback
        });
        let mut hasher = Sha256::new();
        hasher.update(b"CODEX_MODELS_CACHE_IDENTITY_V2\0");
        hasher.update(canonical);
        let digest = hasher.finalize();
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

fn normalize_model_cache_base_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return raw.trim().trim_end_matches('/').to_string();
    };
    url.set_fragment(None);
    let normalized_path = url.path().trim_end_matches('/').to_string();
    url.set_path(if normalized_path.is_empty() { "/" } else { &normalized_path });
    url.to_string()
}

fn model_cache_secret_component(domain: &[u8], value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"CODEX_MODELS_CACHE_SECRET_COMPONENT_V1\0");
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(value.as_bytes());
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Creates the default runtime model provider for configured provider metadata.
pub fn create_model_provider(
    provider_info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
) -> SharedModelProvider {
    if provider_info.is_amazon_bedrock() {
        Arc::new(AmazonBedrockModelProvider::new(provider_info, auth_manager))
    } else {
        Arc::new(ConfiguredModelProvider::new(provider_info, auth_manager))
    }
}

/// Runtime model provider backed by configured `ModelProviderInfo`.
#[derive(Clone, Debug)]
struct ConfiguredModelProvider {
    info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
}

impl ConfiguredModelProvider {
    fn new(provider_info: ModelProviderInfo, auth_manager: Option<Arc<AuthManager>>) -> Self {
        let auth_manager = auth_manager_for_provider(auth_manager, &provider_info);
        Self {
            info: provider_info,
            auth_manager,
        }
    }
}

impl ModelProvider for ConfiguredModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> ProviderCapabilities {
        use codex_model_provider_info::ProviderKind;
        if matches!(
            self.info.kind,
            ProviderKind::Ollama | ProviderKind::LmStudio
        ) {
            ProviderCapabilities {
                namespace_tools: false,
                web_search: false,
                image_generation: false,
            }
        } else {
            ProviderCapabilities {
                web_search: self.info.supports_hosted_web_search,
                ..ProviderCapabilities::default()
            }
        }
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    fn missing_required_credentials(&self) -> bool {
        // External auth resolves its source on every request, so an empty cache
        // does not mean the request will go out unauthenticated.
        provider_uses_first_party_auth_path(&self.info)
            && self.auth_manager.as_ref().is_some_and(|auth_manager| {
                !auth_manager.has_external_auth() && auth_manager.auth_cached().is_none()
            })
    }

    fn supports_attestation(&self) -> bool {
        self.auth_manager
            .as_ref()
            .and_then(|auth_manager| auth_manager.auth_cached())
            .is_some_and(|auth| auth.is_chatgpt_auth())
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(async move {
            match self.auth_manager.as_ref() {
                Some(auth_manager) => auth_manager.auth().await,
                None => None,
            }
        })
    }

    fn resolve_unscoped_client_setup(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<(Provider, SharedAuthProvider)>>
    {
        Box::pin(async move {
            let auth = self.auth().await;
            Ok((
                request_api_provider(&self.info, auth.as_ref().map(CodexAuth::auth_mode))?,
                resolve_provider_auth(auth.as_ref(), &self.info)?,
            ))
        })
    }

    fn resolve_codex_backend_client_setup(
        &self,
    ) -> ModelProviderFuture<
        '_,
        codex_protocol::error::Result<Option<(Provider, SharedAuthProvider)>>,
    > {
        Box::pin(async move {
            let auth = self.auth().await;
            if !self.info.is_openai() || !auth.as_ref().is_some_and(CodexAuth::uses_codex_backend) {
                return Ok(None);
            }
            Ok(Some((
                request_api_provider(&self.info, auth.as_ref().map(CodexAuth::auth_mode))?,
                resolve_provider_auth(auth.as_ref(), &self.info)?,
            )))
        })
    }

    fn resolve_client_setup(
        &self,
        scope: ProviderAuthScope,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<ResolvedModelProviderClientSetup>>
    {
        Box::pin(async move {
            let initial = self
                .auth_manager
                .as_ref()
                .map(|manager| manager.policy_auth_snapshot())
                .transpose()?;
            let auth = self.auth().await;
            let validate_lifecycle = || -> codex_protocol::error::Result<()> {
                if let (Some(manager), Some((epoch, Some(_)))) = (&self.auth_manager, &initial)
                    && manager.policy_auth_snapshot()?.0 != *epoch
                {
                    return Err(CodexErr::UnsupportedOperation(
                        "authentication changed while preparing request; rebuild request state"
                            .into(),
                    ));
                }
                Ok(())
            };
            validate_lifecycle()?;
            let api_provider =
                request_api_provider(&self.info, auth.as_ref().map(CodexAuth::auth_mode))?;
            let resolved_auth = if provider_uses_first_party_auth_path(&self.info) {
                resolve_provider_auth_for_scope(
                    self.auth_manager(),
                    auth.as_ref(),
                    &self.info,
                    scope,
                )
                .await?
            } else {
                ResolvedProviderAuth::new(resolve_provider_auth(auth.as_ref(), &self.info)?)
            };
            validate_lifecycle()?;
            Ok(ResolvedModelProviderClientSetup {
                auth,
                api_provider,
                resolved_auth,
            })
        })
    }

    fn account_state(&self) -> ProviderAccountResult {
        if self.info.requires_openai_auth
            && let Some(manager) = &self.auth_manager
        {
            manager
                .check_auth_load()
                .map_err(|err| ProviderAccountError::CredentialsUnavailable(err.kind()))?;
        }
        let account = if self.info.requires_openai_auth {
            self.auth_manager
                .as_ref()
                .and_then(|auth_manager| {
                    let auth = auth_manager.auth_cached()?;
                    if auth_manager.refresh_failure_for_auth(&auth).is_some() {
                        return None;
                    }
                    if matches!(auth, CodexAuth::Headers(_)) {
                        return None;
                    }
                    Some(auth)
                })
                .map(|auth| match &auth {
                    CodexAuth::ApiKey(_) => Ok(ProviderAccount::ApiKey),
                    CodexAuth::BedrockApiKey(_) => {
                        Err(ProviderAccountError::UnsupportedBedrockApiKeyAuth)
                    }
                    CodexAuth::Chatgpt(_)
                    | CodexAuth::ChatgptAuthTokens(_)
                    | CodexAuth::Headers(_)
                    | CodexAuth::AgentIdentity(_)
                    | CodexAuth::PersonalAccessToken(_) => {
                        let email = auth.get_account_email();
                        let plan_type = auth.account_plan_type();

                        plan_type
                            .map(|plan_type| ProviderAccount::Chatgpt { email, plan_type })
                            .ok_or(ProviderAccountError::MissingChatgptAccountDetails)
                    }
                })
                .transpose()?
        } else {
            None
        };

        Ok(ProviderAccountState {
            account,
            requires_openai_auth: self.info.requires_openai_auth,
        })
    }

    fn models_manager(
        &self,
        model_provider_id: &str,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        if config_model_catalog.is_none()
            && matches!(
                self.info.kind,
                codex_model_provider_info::ProviderKind::Ollama
                    | codex_model_provider_info::ProviderKind::LmStudio
            )
        {
            return Arc::new(crate::local_models::manager(
                model_provider_id,
                self.info.clone(),
                codex_home,
            ));
        }
        match config_model_catalog {
            Some(model_catalog) => Arc::new(StaticModelsManager::new(
                self.auth_manager.clone(),
                model_catalog,
            )),
            None => {
                let endpoint = Arc::new(OpenAiModelsEndpoint::new(
                    model_provider_id.to_owned(),
                    self.info.clone(),
                    self.auth_manager.clone(),
                ));
                let identity_endpoint = Arc::clone(&endpoint);
                let cache_identity: ModelsCacheIdentity =
                    Arc::new(move || identity_endpoint.current_cache_identity());
                Arc::new(OpenAiModelsManager::new(
                    codex_home,
                    endpoint,
                    self.auth_manager.clone(),
                    cache_identity,
                ))
            }
        }
    }
}

