//! Default Codex HTTP client: shared `User-Agent`, `originator`, optional residency header, and
//! reqwest/`HttpClient` construction.
//!
//! Use [`crate::default_client`] or [`codex_login::default_client`] from other crates in this
//! workspace.

use codex_http_client::BuildCustomCaTransportError;
use codex_http_client::BuildRouteAwareHttpClientError;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
pub use codex_http_client::RequestBuilder as CodexRequestBuilder;
use codex_http_client::build_reqwest_client_with_custom_ca;
use codex_http_client::with_chatgpt_cloudflare_cookie_store;
use codex_terminal_detection::user_agent;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::USER_AGENT;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::LazyLock;
use std::sync::RwLock;
use std::sync::RwLockWriteGuard;

use crate::outbound_proxy::AuthRouteConfig;

/// Set this to add a suffix to the User-Agent string.
///
/// It is not ideal that we're using a global singleton for this.
/// This is primarily designed to differentiate MCP clients from each other.
/// Because there can only be one MCP server per process, it should be safe for this to be a global static.
/// However, future users of this should use this with caution as a result.
/// In addition, we want to be confident that this value is used for ALL clients and doing that requires a
/// lot of wiring and it's easy to miss code paths by doing so.
/// See https://github.com/openai/codex/pull/3388/files for an example of what that would look like.
/// Finally, we want to make sure this is set for ALL mcp clients without needing to know a special env var
/// or having to set data that they already specified in the mcp initialize request somewhere else.
///
/// A space is automatically added between the suffix and the rest of the User-Agent string.
/// The full user agent string is returned from the mcp initialize response.
/// Parenthesis will be added by Codex. This should only specify what goes inside of the parenthesis.
pub static USER_AGENT_SUFFIX: UserAgentSuffix = UserAgentSuffix;
pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
pub const CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";
pub const RESIDENCY_HEADER_NAME: &str = "x-openai-internal-codex-residency";

pub use codex_config::ResidencyRequirement;

#[derive(Debug, Clone)]
pub struct Originator {
    pub value: String,
    pub header_value: HeaderValue,
}

#[derive(Default)]
struct ProcessIdentityState {
    originator: Option<Originator>,
    suffix: Option<String>,
}

#[derive(Clone)]
struct ProcessIdentitySnapshot {
    originator: Originator,
    suffix: Option<String>,
}

static PROCESS_IDENTITY: LazyLock<RwLock<ProcessIdentityState>> =
    LazyLock::new(|| RwLock::new(ProcessIdentityState::default()));

pub struct UserAgentSuffix;

#[derive(Debug, Clone, Copy)]
pub struct UserAgentSuffixLockError;

pub struct UserAgentSuffixGuard<'a> {
    guard: RwLockWriteGuard<'a, ProcessIdentityState>,
}

impl UserAgentSuffix {
    pub fn lock(&self) -> Result<UserAgentSuffixGuard<'static>, UserAgentSuffixLockError> {
        PROCESS_IDENTITY
            .write()
            .map(|guard| UserAgentSuffixGuard { guard })
            .map_err(|_| UserAgentSuffixLockError)
    }
}

impl Deref for UserAgentSuffixGuard<'_> {
    type Target = Option<String>;

    fn deref(&self) -> &Self::Target {
        &self.guard.suffix
    }
}

impl DerefMut for UserAgentSuffixGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard.suffix
    }
}
static REQUIREMENTS_RESIDENCY: LazyLock<RwLock<Option<ResidencyRequirement>>> =
    LazyLock::new(|| RwLock::new(None));
static ROUTE_AWARE_CLIENT_BUILD_PERMIT: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(1);

#[derive(Debug)]
pub enum SetOriginatorError {
    InvalidHeaderValue,
    AlreadyInitialized,
}

fn get_originator_value(provided: Option<String>) -> Originator {
    let value = provided.unwrap_or(DEFAULT_ORIGINATOR.to_string());

    match HeaderValue::from_str(&value) {
        Ok(header_value) => Originator {
            value,
            header_value,
        },
        Err(e) => {
            tracing::error!("Unable to turn originator override {value} into header value: {e}");
            Originator {
                value: DEFAULT_ORIGINATOR.to_string(),
                header_value: HeaderValue::from_static(DEFAULT_ORIGINATOR),
            }
        }
    }
}

fn install_process_identity<F>(
    state: &RwLock<ProcessIdentityState>,
    value: String,
    suffix: Option<Option<String>>,
    originator_override: F,
) -> Result<(), SetOriginatorError>
where
    F: FnOnce() -> Option<String>,
{
    if HeaderValue::from_str(&value).is_err() {
        return Err(SetOriginatorError::InvalidHeaderValue);
    }
    let Ok(mut guard) = state.write() else {
        return Err(SetOriginatorError::AlreadyInitialized);
    };
    if guard.originator.is_some() {
        return Err(SetOriginatorError::AlreadyInitialized);
    }
    let originator_override = originator_override();
    guard.originator = Some(get_originator_value(
        originator_override.clone().or(Some(value)),
    ));
    // An environment override owns process identity independently of an
    // app-server request client. Do not pair it with that client's suffix.
    if originator_override.is_none()
        && let Some(suffix) = suffix
    {
        guard.suffix = suffix;
    }
    Ok(())
}

pub fn set_default_process_identity(
    value: String,
    suffix: Option<String>,
) -> Result<(), SetOriginatorError> {
    install_process_identity(&PROCESS_IDENTITY, value, Some(suffix), || {
        std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR).ok()
    })
}

pub fn set_default_originator(value: String) -> Result<(), SetOriginatorError> {
    install_process_identity(&PROCESS_IDENTITY, value, /*suffix*/ None, || {
        std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR).ok()
    })
}

pub fn set_default_client_residency_requirement(enforce_residency: Option<ResidencyRequirement>) {
    let Ok(mut guard) = REQUIREMENTS_RESIDENCY.write() else {
        tracing::warn!("Failed to acquire requirements residency lock");
        return;
    };
    *guard = enforce_residency;
}

fn process_identity_snapshot_from_state<F>(
    state: &RwLock<ProcessIdentityState>,
    originator_override: F,
) -> ProcessIdentitySnapshot
where
    F: FnOnce() -> Option<String>,
{
    let Ok(mut guard) = state.write() else {
        return ProcessIdentitySnapshot {
            originator: get_originator_value(/*provided*/ None),
            suffix: None,
        };
    };
    if guard.originator.is_none()
        && let Some(originator_override) = originator_override()
    {
        guard.originator = Some(get_originator_value(Some(originator_override)));
    }
    ProcessIdentitySnapshot {
        originator: guard
            .originator
            .clone()
            .unwrap_or_else(|| get_originator_value(/*provided*/ None)),
        suffix: guard.suffix.clone(),
    }
}

fn process_identity_snapshot() -> ProcessIdentitySnapshot {
    process_identity_snapshot_from_state(&PROCESS_IDENTITY, || {
        std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR).ok()
    })
}

pub fn originator() -> Originator {
    process_identity_snapshot().originator
}

pub fn is_first_party_originator(originator_value: &str) -> bool {
    originator_value == DEFAULT_ORIGINATOR
        || originator_value == "codex-tui"
        || originator_value == "codex_vscode"
        || originator_value.starts_with("Codex ")
}

pub fn is_first_party_chat_originator(originator_value: &str) -> bool {
    originator_value == "codex_atlas" || originator_value == "codex_chatgpt_desktop"
}

fn codex_user_agent_for_identity(identity: &ProcessIdentitySnapshot) -> String {
    let build_version = env!("CARGO_PKG_VERSION");
    let os_info = os_info::get();
    let prefix = format!(
        "{}/{build_version} ({} {}; {}) {}",
        identity.originator.value.as_str(),
        os_info.os_type(),
        os_info.version(),
        os_info.architecture().unwrap_or("unknown"),
        user_agent()
    );
    let suffix = identity
        .suffix
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(String::new, |value| format!(" ({value})"));

    let candidate = format!("{prefix}{suffix}");
    sanitize_user_agent_with_originator(candidate, &prefix, &identity.originator.value)
}

pub fn get_codex_user_agent() -> String {
    codex_user_agent_for_identity(&process_identity_snapshot())
}

/// Sanitize the user agent string.
///
/// Invalid characters are replaced with an underscore.
///
/// If the user agent fails to parse, it falls back to fallback and then to ORIGINATOR.
#[cfg(test)]
fn sanitize_user_agent(candidate: String, fallback: &str) -> String {
    sanitize_user_agent_with_originator(candidate, fallback, &originator().value)
}

fn sanitize_user_agent_with_originator(
    candidate: String,
    fallback: &str,
    originator: &str,
) -> String {
    if HeaderValue::from_str(candidate.as_str()).is_ok() {
        return candidate;
    }

    let sanitized: String = candidate
        .chars()
        .map(|ch| if matches!(ch, ' '..='~') { ch } else { '_' })
        .collect();
    if !sanitized.is_empty() && HeaderValue::from_str(sanitized.as_str()).is_ok() {
        tracing::warn!(
            "Sanitized Codex user agent because provided suffix contained invalid header characters"
        );
        sanitized
    } else if HeaderValue::from_str(fallback).is_ok() {
        tracing::warn!(
            "Falling back to base Codex user agent because provided suffix could not be sanitized"
        );
        fallback.to_string()
    } else {
        tracing::warn!(
            "Falling back to default Codex originator because base user agent string is invalid"
        );
        originator.to_string()
    }
}

/// Create an HTTP client with default `originator` and `User-Agent` headers set.
///
/// This supported default path preserves reqwest's existing proxy behavior and does not opt into
/// Codex's route-aware system/PAC resolution.
pub fn create_client() -> HttpClient {
    let inner = build_reqwest_client();
    HttpClient::new(inner)
}

/// Builds the default reqwest client used for ordinary Codex HTTP traffic.
///
/// This starts from the standard Codex user agent, default headers, and sandbox-specific proxy
/// policy, then layers in shared custom CA handling from `CODEX_CA_CERTIFICATE` /
/// `SSL_CERT_FILE`. The function remains infallible for compatibility with existing call sites, so
/// a custom-CA or builder failure is logged and falls back to `reqwest::Client::new()`.
///
/// This supported default path preserves reqwest's existing proxy behavior and does not opt into
/// Codex's route-aware system/PAC resolution. Auth callers with route settings must use
/// `build_default_auth_reqwest_client` or `create_default_auth_client`.
pub fn build_reqwest_client() -> reqwest::Client {
    try_build_reqwest_client().unwrap_or_else(|error| {
        tracing::warn!(error = %error, "failed to build default reqwest client");
        with_chatgpt_cloudflare_cookie_store(reqwest::Client::builder())
            .build()
            .unwrap_or_else(|fallback_error| {
                tracing::warn!(
                    error = %fallback_error,
                    "failed to build fallback reqwest client with ChatGPT Cloudflare cookie store"
                );
                reqwest::Client::new()
            })
    })
}

/// Tries to build the default reqwest client used for ordinary Codex HTTP traffic.
///
/// Callers that need a structured CA-loading failure instead of the legacy logged fallback can use
/// this method directly.
pub fn try_build_reqwest_client() -> Result<reqwest::Client, BuildCustomCaTransportError> {
    build_reqwest_client_with_custom_ca(default_reqwest_client_builder())
}

/// Builds the default Codex reqwest client for a concrete outbound route.
///
/// When route-aware proxy handling is disabled, or the client is running inside the Codex
/// sandbox, this preserves the default client's existing proxy behavior. Otherwise it resolves
/// the destination through the shared system/PAC-aware routing policy.
pub fn build_default_reqwest_client_for_route(
    http_client_factory: &HttpClientFactory,
    request_url: &str,
    route_class: ClientRouteClass,
) -> Result<reqwest::Client, BuildRouteAwareHttpClientError> {
    if matches!(
        http_client_factory.outbound_proxy_policy(),
        OutboundProxyPolicy::ReqwestDefault
    ) {
        return Ok(build_reqwest_client());
    }
    if is_sandboxed() {
        // Preserve the sandbox's existing no-proxy policy; sandboxed command egress is routed
        // separately through network-proxy.
        return Ok(build_reqwest_client());
    }

    http_client_factory.build_reqwest_client(
        default_reqwest_client_builder(),
        request_url,
        route_class,
    )
}

/// Builds the default Codex reqwest client for a concrete outbound route without blocking the
/// async runtime worker that initiated the request.
pub async fn build_default_reqwest_client_for_route_async(
    http_client_factory: HttpClientFactory,
    request_url: String,
    route_class: ClientRouteClass,
) -> std::io::Result<reqwest::Client> {
    let permit = ROUTE_AWARE_CLIENT_BUILD_PERMIT
        .acquire()
        .await
        .map_err(std::io::Error::other)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        build_default_reqwest_client_for_route(&http_client_factory, &request_url, route_class)
            .map_err(std::io::Error::from)
    })
    .await
    .map_err(std::io::Error::other)?
}

fn default_reqwest_client_builder() -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder().default_headers(default_headers());
    if is_sandboxed() {
        builder = builder.no_proxy();
    }
    with_chatgpt_cloudflare_cookie_store(builder)
}

/// Builds a raw reqwest client for an auth endpoint without Codex default headers.
pub(crate) fn build_raw_auth_reqwest_client(
    endpoint: &str,
    auth_route_config: Option<&AuthRouteConfig>,
) -> Result<reqwest::Client, BuildRouteAwareHttpClientError> {
    auth_http_client_factory(auth_route_config).build_reqwest_client(
        reqwest::Client::builder(),
        endpoint,
        ClientRouteClass::Auth,
    )
}

/// Builds the default Codex reqwest client for an auth endpoint.
pub(crate) fn build_default_auth_reqwest_client(
    endpoint: &str,
    auth_route_config: Option<&AuthRouteConfig>,
) -> Result<reqwest::Client, BuildRouteAwareHttpClientError> {
    build_default_reqwest_client_for_route(
        &auth_http_client_factory(auth_route_config),
        endpoint,
        ClientRouteClass::Auth,
    )
}

/// Builds the default Codex HTTP client wrapper for an auth endpoint.
pub(crate) fn create_default_auth_client(
    endpoint: &str,
    auth_route_config: Option<&AuthRouteConfig>,
) -> Result<HttpClient, BuildRouteAwareHttpClientError> {
    build_default_auth_reqwest_client(endpoint, auth_route_config).map(HttpClient::new)
}

fn auth_http_client_factory(auth_route_config: Option<&AuthRouteConfig>) -> HttpClientFactory {
    auth_route_config.map_or_else(
        || HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        |config| config.http_client_factory().clone(),
    )
}

pub fn default_headers() -> HeaderMap {
    let identity = process_identity_snapshot();
    let mut headers = HeaderMap::new();
    headers.insert("originator", identity.originator.header_value.clone());
    if let Ok(user_agent) = HeaderValue::from_str(&codex_user_agent_for_identity(&identity)) {
        headers.insert(USER_AGENT, user_agent);
    }
    if let Ok(guard) = REQUIREMENTS_RESIDENCY.read()
        && let Some(requirement) = guard.as_ref()
        && !headers.contains_key(RESIDENCY_HEADER_NAME)
    {
        let value = match requirement {
            ResidencyRequirement::Us => HeaderValue::from_static("us"),
        };
        headers.insert(RESIDENCY_HEADER_NAME, value);
    }
    headers
}

fn is_sandboxed() -> bool {
    std::env::var("CODEX_SANDBOX").as_deref() == Ok("seatbelt")
}

#[cfg(test)]
#[path = "default_client_tests.rs"]
mod tests;
