use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_agent_identity::AgentIdentityKey;
use codex_agent_identity::authorization_header_for_agent_task;
use codex_api::AgentIdentityTelemetry;
use codex_api::AuthError;
use codex_api::AuthProvider;
use codex_api::SharedAuthProvider;
use codex_login::AuthHeaders;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth::AgentIdentityAuth;
use codex_login::auth::AgentIdentityAuthError;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::account::PlanType;
use codex_protocol::auth::AuthMode;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;
use sha2::Digest;
use sha2::Sha256;

use crate::bearer_auth_provider::BearerAuthProvider;

const BEDROCK_API_KEY_UNSUPPORTED_MESSAGE: &str =
    "Bedrock API key auth is only supported by the Amazon Bedrock model provider";
const MODELS_CACHE_AUTH_SCOPE_DOMAIN: &[u8] = b"CODEX_MODELS_CACHE_AUTH_SCOPE_V1\0";
const MODELS_CACHE_CREDENTIAL_SCOPE_DOMAIN: &[u8] = b"CODEX_MODELS_CACHE_CREDENTIAL_SCOPE_V1\0";

/// Account-scoped request authority derived from the same auth snapshot used
/// to construct provider request headers.
///
/// This type deliberately has no `Debug` implementation: its raw values are
/// only transient digest input and must never be logged or persisted.
#[derive(Clone, Eq, PartialEq)]
struct ProviderAccountScope {
    auth_mode: Option<AuthMode>,
    account_id: Option<Vec<u8>>,
    chatgpt_user_id: Option<Vec<u8>>,
    account_plan_type: Option<PlanType>,
    workspace_account: Option<bool>,
    fedramp_header: Option<Vec<u8>>,
    credential_scope_digest: Option<Vec<u8>>,
    routing_headers_digest: Option<Vec<u8>>,
}

impl ProviderAccountScope {
    fn from_auth(auth: Option<&CodexAuth>) -> Self {
        let Some(auth) = auth else {
            return Self {
                auth_mode: None,
                account_id: None,
                chatgpt_user_id: None,
                account_plan_type: None,
                workspace_account: None,
                fedramp_header: None,
                credential_scope_digest: None,
                routing_headers_digest: None,
            };
        };

        let (account_id, fedramp_header, workspace_account) = match auth {
            CodexAuth::Headers(auth) => (
                auth.headers()
                    .get("ChatGPT-Account-ID")
                    .map(|value| value.as_bytes().to_vec()),
                auth.headers()
                    .get("X-OpenAI-Fedramp")
                    .map(|value| value.as_bytes().to_vec()),
                None,
            ),
            CodexAuth::ApiKey(_) | CodexAuth::BedrockApiKey(_) => (None, None, None),
            CodexAuth::Chatgpt(_)
            | CodexAuth::ChatgptAuthTokens(_)
            | CodexAuth::AgentIdentity(_)
            | CodexAuth::PersonalAccessToken(_) => (
                auth.get_account_id().map(String::into_bytes),
                auth.is_fedramp_account().then(|| b"true".to_vec()),
                Some(auth.is_workspace_account()),
            ),
        };

        let chatgpt_user_id = auth.get_chatgpt_user_id().map(String::into_bytes);
        let credential_scope_digest = if account_id.is_none() && chatgpt_user_id.is_none() {
            match auth {
                CodexAuth::Headers(auth) => {
                    Some(credential_scope_digest_for_headers(auth.headers()))
                }
                CodexAuth::ApiKey(_) => auth
                    .get_token()
                    .ok()
                    .map(|token| credential_scope_digest(token.as_bytes())),
                CodexAuth::BedrockApiKey(auth) => {
                    Some(credential_scope_digest(auth.api_key.as_bytes()))
                }
                CodexAuth::Chatgpt(_)
                | CodexAuth::ChatgptAuthTokens(_)
                | CodexAuth::PersonalAccessToken(_) => auth
                    .get_token()
                    .ok()
                    .map(|token| credential_scope_digest(token.as_bytes())),
                CodexAuth::AgentIdentity(_) => None,
            }
        } else {
            None
        };

        let credential_scope_digest = auth
            .lifecycle_id()
            .map(|lifecycle| crate::auth::credential_scope_digest(lifecycle.as_bytes()))
            .or(credential_scope_digest);
        Self {
            auth_mode: Some(auth.auth_mode()),
            account_id,
            chatgpt_user_id,
            account_plan_type: auth.account_plan_type(),
            workspace_account,
            fedramp_header,
            credential_scope_digest,
            routing_headers_digest: match auth {
                CodexAuth::Headers(auth) => Some(header_scope_digest(auth.headers(), true)),
                _ => None,
            },
        }
    }
}

fn credential_scope_digest(credential: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(MODELS_CACHE_CREDENTIAL_SCOPE_DOMAIN);
    hasher.update(credential);
    hasher.finalize().to_vec()
}

fn credential_scope_digest_for_headers(headers: &HeaderMap) -> Vec<u8> {
    header_scope_digest(headers, false)
}

fn header_scope_digest(headers: &HeaderMap, routing_only: bool) -> Vec<u8> {
    let mut names = headers
        .keys()
        .filter(|name| !routing_only || *name != http::header::AUTHORIZATION)
        .collect::<Vec<_>>();
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    let entries = names
        .into_iter()
        .flat_map(|name| {
            headers
                .get_all(name)
                .iter()
                .map(move |value| (name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()))
        })
        .collect::<Vec<_>>();
    let mut canonical = Vec::new();
    for (name, value) in entries {
        canonical.extend_from_slice(&(name.len() as u64).to_le_bytes());
        canonical.extend_from_slice(&name);
        canonical.extend_from_slice(&(value.len() as u64).to_le_bytes());
        canonical.extend_from_slice(&value);
    }
    credential_scope_digest(&canonical)
}

/// Non-sensitive material incorporated into the complete model-cache identity.
pub(crate) struct ProviderCacheAuthIdentity {
    pub(crate) auth_mode: Option<AuthMode>,
    pub(crate) account_scope_digest: String,
}

/// Returns a stable digest of the current authoritative account/workspace
/// scope. Raw account and user identifiers never leave this function.
pub(crate) fn provider_cache_auth_identity(
    auth_manager: Option<&AuthManager>,
) -> ProviderCacheAuthIdentity {
    let auth = auth_manager.and_then(AuthManager::auth_cached);
    provider_cache_auth_identity_for_auth(auth.as_ref(), auth_manager)
}

pub(crate) fn provider_cache_auth_identity_for_auth(
    auth: Option<&CodexAuth>,
    auth_manager: Option<&AuthManager>,
) -> ProviderCacheAuthIdentity {
    let account_scope = ProviderAccountScope::from_auth(auth);
    let auth_mode = account_scope.auth_mode;
    let effective_chatgpt_workspaces = auth_manager
        .and_then(AuthManager::effective_chatgpt_workspaces)
        .map(|mut workspaces| {
            workspaces.sort();
            workspaces.dedup();
            workspaces
        });
    let canonical = serde_json::to_vec(&serde_json::json!({
        "version": 3,
        "auth_mode": account_scope.auth_mode,
        "account_id": account_scope.account_id,
        "chatgpt_user_id": account_scope.chatgpt_user_id,
        "account_plan_type": account_scope.account_plan_type,
        "workspace_account": account_scope.workspace_account,
        "fedramp_header": account_scope.fedramp_header,
        "credential_scope_digest": account_scope.credential_scope_digest,
        "routing_headers_digest": account_scope.routing_headers_digest,
        "effective_chatgpt_workspaces": effective_chatgpt_workspaces,
    }))
    .unwrap_or_else(|error| {
        let mut fallback = b"\0provider-auth-cache-identity-serialization-error:".to_vec();
        fallback.extend_from_slice(error.to_string().as_bytes());
        fallback
    });
    let mut hasher = Sha256::new();
    hasher.update(MODELS_CACHE_AUTH_SCOPE_DOMAIN);
    hasher.update(canonical);
    let account_scope_digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    ProviderCacheAuthIdentity {
        auth_mode,
        account_scope_digest,
    }
}

#[derive(Clone, Debug)]
pub struct ProviderAuthScope {
    pub agent_identity_policy: AgentIdentityAuthPolicy,
    pub session_source: SessionSource,
    pub agent_identity_session_fallback: AgentIdentitySessionFallback,
}

#[derive(Clone, Debug, Default)]
pub struct AgentIdentitySessionFallback {
    engaged: Arc<AtomicBool>,
}

impl AgentIdentitySessionFallback {
    pub fn is_engaged(&self) -> bool {
        self.engaged.load(Ordering::Relaxed)
    }

    fn engage(&self) -> bool {
        !self.engaged.swap(true, Ordering::Relaxed)
    }
}

/// Provider auth resolved for a request, plus metadata describing the effective auth.
#[derive(Clone)]
pub struct ResolvedProviderAuth {
    pub auth: SharedAuthProvider,
    pub agent_identity_telemetry: Option<AgentIdentityTelemetry>,
}

impl ResolvedProviderAuth {
    pub(crate) fn new(auth: SharedAuthProvider) -> Self {
        Self {
            auth,
            agent_identity_telemetry: None,
        }
    }

    fn for_agent_identity(auth: AgentIdentityAuth) -> Self {
        let agent_identity_telemetry = agent_identity_telemetry(&auth);
        Self {
            auth: Arc::new(AgentIdentityAuthProvider { auth }),
            agent_identity_telemetry: Some(agent_identity_telemetry),
        }
    }
}

pub(crate) fn agent_identity_telemetry(auth: &AgentIdentityAuth) -> AgentIdentityTelemetry {
    AgentIdentityTelemetry {
        agent_id: auth.record().agent_runtime_id.clone(),
        task_id: auth.run_task_id().to_string(),
    }
}

#[derive(Clone, Debug)]
struct AgentIdentityAuthProvider {
    auth: AgentIdentityAuth,
}

impl AuthProvider for AgentIdentityAuthProvider {
    fn auth_header_telemetry(&self) -> codex_api::AuthHeaderTelemetry {
        // AgentIdentityAuth validates its immutable private key in from_record.
        // Assertions serialize to ASCII; the independently parsed account
        // header is the remaining fallible header step. Dispatch still signs
        // freshly through try_add_auth_headers, never through telemetry.
        let attached = HeaderValue::from_str(self.auth.account_id()).is_ok();
        codex_api::AuthHeaderTelemetry {
            attached,
            name: attached.then_some("authorization"),
        }
    }

    fn connection_identity(&self) -> Option<Vec<u8>> {
        let record = serde_json::to_vec(self.auth.record()).ok()?;
        let mut digest = Sha256::new();
        digest.update(b"CODEX_AGENT_CONNECTION_IDENTITY_V1\0");
        digest.update(record);
        Some(digest.finalize().to_vec())
    }

    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let _ = self.try_add_auth_headers(headers);
    }

    fn try_add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), AuthError> {
        let record = self.auth.record();
        let value = authorization_header_for_agent_task(
            AgentIdentityKey {
                agent_runtime_id: &record.agent_runtime_id,
                private_key_pkcs8_base64: &record.agent_private_key,
            },
            self.auth.run_task_id(),
        )
        .map_err(|error| AuthError::Build(format!("failed to build agent assertion: {error}")))?;
        let mut authorization = HeaderValue::from_str(&value)
            .map_err(|_| AuthError::Build("invalid agent authorization header".into()))?;
        authorization.set_sensitive(true);
        let account = HeaderValue::from_str(self.auth.account_id())
            .map_err(|_| AuthError::Build("invalid agent account header".into()))?;
        headers.insert(http::header::AUTHORIZATION, authorization);
        headers.insert("ChatGPT-Account-ID", account);
        if self.auth.is_fedramp_account() {
            headers.insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct HeaderAuthProvider {
    auth: AuthHeaders,
}

impl AuthProvider for HeaderAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let mut auth_headers = self.auth.headers().clone();
        for value in auth_headers.values_mut() {
            value.set_sensitive(true);
        }
        headers.extend(auth_headers);
    }
}

struct AuthManagerAuthProvider {
    auth_manager: Arc<AuthManager>,
    // Startup auth is only the account-scoped identity anchor. Request
    // headers always come from the current AuthManager snapshot below.
    expected_scope: ProviderAccountScope,
    expected_epoch: u64,
    projection: Option<SharedAuthProvider>,
}

struct ManagedDispatchGuard {
    manager: Arc<AuthManager>,
    epoch: u64,
}

impl codex_http_client::RequestDispatchGuard for ManagedDispatchGuard {
    fn admit(&self, dispatch: &mut dyn FnMut()) -> Result<(), String> {
        self.manager
            .with_policy_auth_epoch(self.epoch, dispatch)
            .map_err(|error| error.to_string())
    }
}

impl AuthManagerAuthProvider {
    fn with_projection<T>(
        &self,
        apply: impl FnOnce(&dyn AuthProvider) -> Result<T, AuthError>,
    ) -> Result<T, AuthError> {
        self.auth_manager
            .check_auth_load()
            .map_err(|err| AuthError::Build(err.to_string()))?;
        let (epoch, auth) = self
            .auth_manager
            .policy_auth_snapshot()
            .map_err(|err| AuthError::Build(err.to_string()))?;
        let auth = auth.ok_or_else(|| {
            AuthError::Build("managed authentication is no longer available".into())
        })?;
        if epoch != self.expected_epoch
            || ProviderAccountScope::from_auth(Some(&auth)) != self.expected_scope
        {
            return Err(AuthError::Build("authentication account scope changed or lifecycle invalidated; rebuild request state".into()));
        }
        let projection = self
            .projection
            .clone()
            .unwrap_or_else(|| auth_provider_from_auth(&auth));
        self.auth_manager
            .with_policy_auth_epoch(epoch, || apply(projection.as_ref()))
            .map_err(|err| AuthError::Build(err.to_string()))?
    }
}

impl AuthProvider for AuthManagerAuthProvider {
    fn dispatch_guard(&self) -> Option<Arc<dyn codex_http_client::RequestDispatchGuard>> {
        Some(Arc::new(ManagedDispatchGuard {
            manager: Arc::clone(&self.auth_manager),
            epoch: self.expected_epoch,
        }))
    }

    fn connection_identity(&self) -> Option<Vec<u8>> {
        self.projection
            .as_ref()
            .and_then(|projection| projection.connection_identity())
    }

    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        if self.try_add_auth_headers(headers).is_err() {
            headers.remove(http::header::AUTHORIZATION);
            headers.remove("ChatGPT-Account-ID");
            headers.remove("X-OpenAI-Fedramp");
        }
    }

    fn auth_header_telemetry(&self) -> codex_api::AuthHeaderTelemetry {
        self.with_projection(|projection| Ok(projection.auth_header_telemetry()))
            .unwrap_or_default()
    }

    fn try_add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), AuthError> {
        self.with_projection(|projection| projection.try_add_auth_headers(headers))
    }
}

// Preserve token retrieval failures until the fallible dispatch boundary.
struct SnapshotBearerAuthProvider {
    auth: CodexAuth,
}

impl AuthProvider for SnapshotBearerAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let _ = self.try_add_auth_headers(headers);
    }

    fn try_add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), AuthError> {
        let token = self.auth.get_token().map_err(|error| {
            AuthError::Build(format!("failed to read authentication token: {error}"))
        })?;
        BearerAuthProvider {
            token: Some(token),
            account_id: self.auth.get_account_id(),
            is_fedramp_account: self.auth.is_fedramp_account(),
        }
        .try_add_auth_headers(headers)
    }
}

// Some providers are meant to send no auth headers. Examples include local OSS
// providers and custom test providers with `requires_openai_auth = false`.
#[derive(Clone, Debug)]
struct UnauthenticatedAuthProvider;

impl AuthProvider for UnauthenticatedAuthProvider {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

pub fn unauthenticated_auth_provider() -> SharedAuthProvider {
    Arc::new(UnauthenticatedAuthProvider)
}

/// Returns the provider-scoped auth manager when this provider uses command-backed auth.
///
/// Providers without custom auth continue using the caller-supplied base manager, when present.
pub(crate) fn auth_manager_for_provider(
    auth_manager: Option<Arc<AuthManager>>,
    provider: &ModelProviderInfo,
) -> Option<Arc<AuthManager>> {
    match provider.auth.clone() {
        Some(config) => Some(AuthManager::external_bearer_only(config)),
        None => auth_manager,
    }
}

pub(crate) fn resolve_provider_auth(
    auth: Option<&CodexAuth>,
    provider: &ModelProviderInfo,
) -> codex_protocol::error::Result<SharedAuthProvider> {
    if let Some(auth) = bearer_auth_for_provider(provider)? {
        return Ok(Arc::new(auth));
    }

    if matches!(auth, Some(CodexAuth::BedrockApiKey(_))) {
        return Err(CodexErr::UnsupportedOperation(
            BEDROCK_API_KEY_UNSUPPORTED_MESSAGE.to_string(),
        ));
    }

    Ok(match auth {
        Some(auth) => auth_provider_from_auth(auth),
        None => unauthenticated_auth_provider(),
    })
}

pub(crate) async fn resolve_provider_auth_for_scope(
    auth_manager: Option<Arc<AuthManager>>,
    auth: Option<&CodexAuth>,
    provider: &ModelProviderInfo,
    scope: ProviderAuthScope,
) -> codex_protocol::error::Result<ResolvedProviderAuth> {
    // Provider-owned overrides are independent of the local Codex account.
    if let Some(manager) = &auth_manager {
        manager.check_auth_load()?;
    }
    if let Some(override_auth) = bearer_auth_for_provider(provider)? {
        return Ok(ResolvedProviderAuth::new(Arc::new(override_auth)));
    }
    let epoch = auth_manager
        .as_ref()
        .map(|manager| manager.policy_auth_snapshot().map(|(epoch, _)| epoch))
        .transpose()?;
    let mut resolved =
        resolve_provider_auth_for_scope_inner(auth_manager.clone(), auth, provider, scope).await?;
    if let (Some(manager), Some(expected), Some(expected_epoch)) = (auth_manager, auth, epoch) {
        let projection = (resolved.agent_identity_telemetry.is_some()
            || matches!(expected, CodexAuth::AgentIdentity(_)))
        .then(|| resolved.auth.clone());
        resolved.auth = Arc::new(AuthManagerAuthProvider {
            auth_manager: manager,
            expected_scope: ProviderAccountScope::from_auth(Some(expected)),
            expected_epoch,
            projection,
        });
    }
    Ok(resolved)
}
async fn resolve_provider_auth_for_scope_inner(
    auth_manager: Option<Arc<AuthManager>>,
    auth: Option<&CodexAuth>,
    provider: &ModelProviderInfo,
    scope: ProviderAuthScope,
) -> codex_protocol::error::Result<ResolvedProviderAuth> {
    let ProviderAuthScope {
        agent_identity_policy,
        session_source,
        agent_identity_session_fallback,
    } = scope;
    if let Some(CodexAuth::AgentIdentity(agent_identity_auth)) = auth {
        return Ok(ResolvedProviderAuth::for_agent_identity(
            agent_identity_auth.clone(),
        ));
    }

    if !should_bootstrap_chatgpt_agent_identity(agent_identity_policy, auth)
        || agent_identity_session_fallback.is_engaged()
    {
        return resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new);
    }

    let Some(auth_manager) = auth_manager else {
        return resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new);
    };

    match auth_manager
        .agent_identity_auth(agent_identity_policy, session_source)
        .await
    {
        Ok(Some(agent_identity_auth)) => Ok(ResolvedProviderAuth::for_agent_identity(
            agent_identity_auth,
        )),
        Ok(None) => resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new),
        Err(err) => {
            if let Some(AgentIdentityAuthError::BootstrapUnavailable {
                operation,
                attempts,
                message,
            }) = err
                .get_ref()
                .and_then(|source| source.downcast_ref::<AgentIdentityAuthError>())
            {
                let newly_engaged = agent_identity_session_fallback.engage();
                tracing::warn!(
                    operation,
                    attempts = *attempts,
                    error = %message,
                    newly_engaged,
                    "agent identity bootstrap unavailable; using ChatGPT bearer auth for this session"
                );
                resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new)
            } else {
                Err(err.into())
            }
        }
    }
}

fn should_bootstrap_chatgpt_agent_identity(
    agent_identity_policy: AgentIdentityAuthPolicy,
    auth: Option<&CodexAuth>,
) -> bool {
    agent_identity_policy == AgentIdentityAuthPolicy::ChatGptAuth
        && matches!(auth, Some(CodexAuth::Chatgpt(_)))
}

fn bearer_auth_for_provider(
    provider: &ModelProviderInfo,
) -> codex_protocol::error::Result<Option<BearerAuthProvider>> {
    if let Some(api_key) = provider.api_key()? {
        return Ok(Some(BearerAuthProvider::new(api_key)));
    }

    if let Some(token) = provider.experimental_bearer_token.clone() {
        return Ok(Some(BearerAuthProvider::new(token)));
    }

    Ok(None)
}

/// Builds request-header auth for a first-party Codex auth snapshot.
pub fn auth_provider_from_auth(auth: &CodexAuth) -> SharedAuthProvider {
    match auth {
        CodexAuth::AgentIdentity(auth) => {
            Arc::new(AgentIdentityAuthProvider { auth: auth.clone() })
        }
        CodexAuth::Headers(auth) => Arc::new(HeaderAuthProvider { auth: auth.clone() }),
        CodexAuth::BedrockApiKey(_) => unreachable!("{BEDROCK_API_KEY_UNSUPPORTED_MESSAGE}"),
        CodexAuth::ApiKey(_)
        | CodexAuth::Chatgpt(_)
        | CodexAuth::ChatgptAuthTokens(_)
        | CodexAuth::PersonalAccessToken(_) => {
            Arc::new(SnapshotBearerAuthProvider { auth: auth.clone() })
        }
    }
}

/// Builds request-header auth that reads the current managed auth snapshot on
/// every request while remaining scoped to the expected auth identity.
///
/// Callers with account-scoped state should pass the same snapshot that keyed
/// that state so a later account switch cannot reuse it.
pub fn auth_provider_from_auth_manager(
    auth_manager: Arc<AuthManager>,
    expected_auth: &CodexAuth,
) -> SharedAuthProvider {
    let expected_epoch = auth_manager
        .policy_auth_snapshot()
        .map(|(epoch, _)| epoch)
        .unwrap_or(u64::MAX);
    Arc::new(AuthManagerAuthProvider {
        auth_manager,
        expected_scope: ProviderAccountScope::from_auth(Some(expected_auth)),
        expected_epoch,
        projection: None,
    })
}

