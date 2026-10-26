use std::env;
use std::future::Future;
use std::sync::Arc;

use codex_agent_identity::AgentIdentityKey;
use codex_agent_identity::ChatGptEnvironment;
use codex_agent_identity::agent_identity_jwks_url;
use codex_agent_identity::agent_registration_url;
use codex_agent_identity::agent_task_registration_url;
use codex_agent_identity::build_abom;
use codex_agent_identity::fetch_agent_identity_jwks;
use codex_agent_identity::generate_agent_key_material;
use codex_agent_identity::is_retryable_registration_error;
use codex_agent_identity::public_key_ssh_from_private_key_pkcs8_base64;
use codex_agent_identity::register_agent_identity;
use codex_agent_identity::register_agent_task;
use codex_agent_identity::verify_agent_identity_jwt;
use codex_http_client::HttpClient;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::protocol::SessionSource;
use thiserror::Error;

use crate::default_client::create_default_auth_client;
use crate::outbound_proxy::AuthRouteConfig;

use super::storage::AgentIdentityAuthRecord;

pub(super) const MAX_AGENT_IDENTITY_BOOTSTRAP_ATTEMPTS: usize = 3;
const CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL_ENV_VAR: &str = "CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL";
const CODEX_AGENT_IDENTITY_JWKS_BASE_URL_ENV_VAR: &str = "CODEX_AGENT_IDENTITY_JWKS_BASE_URL";

fn agent_identity_endpoint_override(environment_variable: &str) -> Option<String> {
    env::var(environment_variable)
        .ok()
        .map(|base_url| base_url.trim().trim_end_matches('/').to_string())
        .filter(|base_url| !base_url.is_empty())
}

fn agent_identity_jwks_base_url_matches(chatgpt_base_url: &str, jwks_base_url: &str) -> bool {
    chatgpt_base_url.trim().trim_end_matches('/') == jwks_base_url
}

pub(super) fn agent_identity_authapi_base_url(
    chatgpt_base_url: Option<&str>,
) -> std::io::Result<String> {
    let environment = match chatgpt_base_url {
        Some(chatgpt_base_url) => ChatGptEnvironment::from_chatgpt_base_url(chatgpt_base_url),
        None => Ok(ChatGptEnvironment::default()),
    };
    let authapi_base_url =
        agent_identity_endpoint_override(CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL_ENV_VAR);
    let jwks_base_url =
        agent_identity_endpoint_override(CODEX_AGENT_IDENTITY_JWKS_BASE_URL_ENV_VAR);

    match (environment, authapi_base_url) {
        (Ok(_), Some(base_url)) => Ok(base_url),
        (Ok(environment), None) => Ok(environment.agent_identity_authapi_base_url().to_string()),
        (Err(_), Some(base_url))
            if chatgpt_base_url.is_some_and(|chatgpt_base_url| {
                jwks_base_url.as_deref().is_some_and(|jwks_base_url| {
                    agent_identity_jwks_base_url_matches(chatgpt_base_url, jwks_base_url)
                })
            }) =>
        {
            Ok(base_url)
        }
        (Err(error), _) => Err(std::io::Error::other(error)),
    }
}

pub(super) fn require_agent_identity_authapi_base_url(
    agent_identity_authapi_base_url: Option<&str>,
) -> std::io::Result<&str> {
    agent_identity_authapi_base_url.ok_or_else(|| {
        std::io::Error::other(
            "Agent Identity only supports production and staging ChatGPT environments",
        )
    })
}

#[derive(Clone, Debug, Error)]
pub enum AgentIdentityAuthError {
    #[error(
        "agent identity bootstrap unavailable after {attempts} attempts during {operation}: {message}"
    )]
    BootstrapUnavailable {
        operation: &'static str,
        attempts: usize,
        message: String,
    },
}

impl AgentIdentityAuthError {
    pub(super) fn bootstrap_unavailable(error: &std::io::Error) -> Option<&Self> {
        match error
            .get_ref()
            .and_then(|source| source.downcast_ref::<Self>())
        {
            Some(error @ Self::BootstrapUnavailable { .. }) => Some(error),
            None => None,
        }
    }
}

#[derive(Debug, Error)]
#[error("retryable agent identity registration failure: {message}")]
pub(super) struct RetryableAgentIdentityRegistrationError {
    message: String,
}

impl RetryableAgentIdentityRegistrationError {
    pub(super) fn new(message: String) -> Self {
        Self { message }
    }
}

#[derive(Clone, Debug)]
pub struct AgentIdentityAuth {
    record: Arc<AgentIdentityAuthRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ManagedChatGptAgentIdentityBinding {
    pub(super) account_id: String,
    pub(super) chatgpt_user_id: String,
    pub(super) email: Option<String>,
    pub(super) plan_type: AccountPlanType,
    pub(super) chatgpt_account_is_fedramp: bool,
    pub(super) access_token: String,
}

impl AgentIdentityAuth {
    pub async fn from_record(
        mut record: AgentIdentityAuthRecord,
        agent_identity_authapi_base_url: &str,
        auth_route_config: &AuthRouteConfig,
    ) -> std::io::Result<Self> {
        if record.issuer_origin.as_deref() != Some(agent_identity_authapi_base_url.trim_end_matches('/'))
        {
            return Err(std::io::Error::other("agent identity belongs to a different auth-service origin"));
        }
        public_key_ssh_from_private_key_pkcs8_base64(&record.agent_private_key)
            .map_err(std::io::Error::other)?;
        if record_needs_task_registration(&record) {
            record.task_id = Some(
                register_task_for_record_with_retries(
                    &record,
                    agent_identity_authapi_base_url,
                    auth_route_config,
                )
                .await?,
            );
        }
        Ok(Self {
            record: Arc::new(record),
        })
    }

    pub async fn from_jwt(
        jwt: &str,
        chatgpt_base_url: &str,
        agent_identity_authapi_base_url: &str,
        auth_route_config: &AuthRouteConfig,
    ) -> std::io::Result<Self> {
        let mut record = verified_record_from_jwt(jwt, chatgpt_base_url, auth_route_config).await?;
        record.issuer_origin = Some(agent_identity_authapi_base_url.trim_end_matches('/').to_string());
        Self::from_record(record, agent_identity_authapi_base_url, auth_route_config).await
    }

    pub fn record(&self) -> &AgentIdentityAuthRecord {
        self.record.as_ref()
    }

    pub fn run_task_id(&self) -> &str {
        match self.record.task_id.as_deref() {
            Some(task_id) => task_id,
            None => unreachable!("AgentIdentityAuth should only be constructed with a task_id"),
        }
    }

    pub fn account_id(&self) -> &str {
        &self.record.account_id
    }

    pub fn chatgpt_user_id(&self) -> &str {
        &self.record.chatgpt_user_id
    }

    pub fn email(&self) -> Option<&str> {
        self.record.email.as_deref()
    }

    pub fn plan_type(&self) -> AccountPlanType {
        self.record.plan_type
    }

    pub fn is_fedramp_account(&self) -> bool {
        self.record.chatgpt_account_is_fedramp
    }
}

pub(super) async fn register_managed_chatgpt_agent_identity(
    binding: ManagedChatGptAgentIdentityBinding,
    agent_identity_authapi_base_url: &str,
    session_source: SessionSource,
    auth_route_config: &AuthRouteConfig,
) -> std::io::Result<AgentIdentityAuthRecord> {
    let key_material = generate_agent_key_material().map_err(std::io::Error::other)?;
    let registration_url = agent_registration_url(agent_identity_authapi_base_url);
    let client = create_default_auth_client(&registration_url, auth_route_config).await?;
    let runtime_id = retry_registration(|| async {
        register_agent_identity(
            &client,
            agent_identity_authapi_base_url,
            &binding.access_token,
            binding.chatgpt_account_is_fedramp,
            &key_material,
            build_abom(session_source.clone()),
            vec!["responsesapi".to_string()],
        )
        .await
        .map_err(|err| {
            if is_retryable_registration_error(&err) {
                std::io::Error::other(RetryableAgentIdentityRegistrationError::new(
                    err.to_string(),
                ))
            } else {
                std::io::Error::other(err)
            }
        })
    })
    .await
    .map_err(|err| classify_bootstrap_error("agent identity registration", err))?;

    let record = AgentIdentityAuthRecord {
        issuer_origin: Some(agent_identity_authapi_base_url.trim_end_matches('/').to_string()),
        agent_runtime_id: runtime_id,
        agent_private_key: key_material.private_key_pkcs8_base64,
        account_id: binding.account_id,
        chatgpt_user_id: binding.chatgpt_user_id,
        email: binding.email,
        plan_type: binding.plan_type,
        chatgpt_account_is_fedramp: binding.chatgpt_account_is_fedramp,
        task_id: None,
    };
    // The caller durably saves this successful stage before task registration.
    Ok(record)
}

pub(super) async fn verified_record_from_jwt(
    jwt: &str,
    chatgpt_base_url: &str,
    auth_route_config: &AuthRouteConfig,
) -> std::io::Result<AgentIdentityAuthRecord> {
    AgentIdentityAuthRecord::from_agent_identity_jwt(jwt)?;
    let jwks_base_url =
        match agent_identity_endpoint_override(CODEX_AGENT_IDENTITY_JWKS_BASE_URL_ENV_VAR) {
            Some(base_url) => {
                if !agent_identity_jwks_base_url_matches(chatgpt_base_url, &base_url) {
                    ChatGptEnvironment::from_chatgpt_base_url(chatgpt_base_url)
                        .map_err(std::io::Error::other)?;
                }
                base_url
            }
            None => chatgpt_base_url.to_string(),
        };
    let jwks_url = agent_identity_jwks_url(&jwks_base_url);
    let client = create_default_auth_client(&jwks_url, auth_route_config).await?;
    let jwks = fetch_agent_identity_jwks(&client, &jwks_base_url)
        .await
        .map_err(std::io::Error::other)?;
    let claims = verify_agent_identity_jwt(jwt, &jwks).map_err(std::io::Error::other)?;
    Ok(claims.into())
}

pub(super) fn record_needs_task_registration(record: &AgentIdentityAuthRecord) -> bool {
    record
        .task_id
        .as_deref()
        .is_none_or(|task_id| task_id.trim().is_empty())
}

pub(super) fn record_matches_managed_chatgpt_binding(
    record: &AgentIdentityAuthRecord,
    binding: &ManagedChatGptAgentIdentityBinding,
    issuer_origin: &str,
) -> bool {
    record.issuer_origin.as_deref() == Some(issuer_origin.trim_end_matches('/'))
        && record.account_id == binding.account_id
        && record.chatgpt_user_id == binding.chatgpt_user_id
        && public_key_ssh_from_private_key_pkcs8_base64(&record.agent_private_key).is_ok()
}

pub(super) fn classify_bootstrap_error(
    operation: &'static str,
    err: std::io::Error,
) -> std::io::Error {
    if is_retryable_io_registration_error(&err) {
        std::io::Error::other(AgentIdentityAuthError::BootstrapUnavailable {
            operation,
            attempts: MAX_AGENT_IDENTITY_BOOTSTRAP_ATTEMPTS,
            message: err.to_string(),
        })
    } else {
        err
    }
}

pub(super) fn is_retryable_io_registration_error(err: &std::io::Error) -> bool {
    err.get_ref().is_some_and(
        <dyn std::error::Error + std::marker::Send + std::marker::Sync + 'static>::is::<
            RetryableAgentIdentityRegistrationError,
        >,
    )
}

pub(super) async fn retry_registration<T, F, Fut>(mut operation: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    let mut attempt = 1;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err)
                if attempt < MAX_AGENT_IDENTITY_BOOTSTRAP_ATTEMPTS
                    && is_retryable_io_registration_error(&err) =>
            {
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_AGENT_IDENTITY_BOOTSTRAP_ATTEMPTS,
                    error = %err,
                    "agent identity registration attempt failed; retrying"
                );
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

async fn register_task_for_record_with_retries(
    record: &AgentIdentityAuthRecord,
    agent_identity_authapi_base_url: &str,
    auth_route_config: &AuthRouteConfig,
) -> std::io::Result<String> {
    let task_registration_url =
        agent_task_registration_url(agent_identity_authapi_base_url, &record.agent_runtime_id);
    let client = create_default_auth_client(&task_registration_url, auth_route_config).await?;
    retry_registration(|| async {
        register_task_for_record(&client, record, agent_identity_authapi_base_url).await
    })
    .await
}

async fn register_task_for_record(
    client: &HttpClient,
    record: &AgentIdentityAuthRecord,
    agent_identity_authapi_base_url: &str,
) -> std::io::Result<String> {
    register_agent_task(
        client,
        agent_identity_authapi_base_url,
        key_for_record(record),
    )
    .await
    .map_err(|err| {
        if is_retryable_registration_error(&err) {
            std::io::Error::other(RetryableAgentIdentityRegistrationError::new(
                err.to_string(),
            ))
        } else {
            std::io::Error::other(err)
        }
    })
}

fn key_for_record(record: &AgentIdentityAuthRecord) -> AgentIdentityKey<'_> {
    AgentIdentityKey {
        agent_runtime_id: &record.agent_runtime_id,
        private_key_pkcs8_base64: &record.agent_private_key,
    }
}

