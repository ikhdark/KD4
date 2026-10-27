//! Conservative outbound proxy selection for resolver-aware HTTP clients.
//!
//! When enabled, platform system discovery is tried first, explicit environment
//! proxies are the fallback, and the final fallback is a direct connection.
//! When disabled, callers retain the existing reqwest builder behavior.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tokio::sync::Semaphore;

use http::HeaderValue;

use crate::chatgpt_cloudflare_cookies::ChatGptCookieStore;
use crate::custom_ca::BuildCustomCaTransportError;
use crate::custom_ca::CustomCaPolicy;
use crate::custom_ca::build_reqwest_client_with_custom_ca_policy;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;

const SYSTEM_PROXY_SUCCESS_CACHE_TTL: Duration = Duration::from_secs(60);
const SYSTEM_PROXY_UNAVAILABLE_CACHE_TTL: Duration = Duration::from_secs(5);
const SYSTEM_PROXY_CACHE_MAX_ENTRIES: usize = 256;
#[cfg(any(target_os = "windows", target_os = "macos"))]
static ASYNC_SYSTEM_PROXY_RESOLUTION_PERMIT: Semaphore = Semaphore::const_new(1);

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Coarse semantic bucket for the HTTP or WebSocket client being constructed.
///
/// This is not the selected proxy route or a concrete endpoint. It labels the
/// product path that owns the client so proxy-resolution diagnostics can
/// distinguish auth, API, WebSocket, and miscellaneous traffic without exposing
/// endpoint details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRouteClass {
    /// Login, token refresh/revoke, PAT, and agent identity auth traffic.
    Auth,
    /// First-party API traffic that is not part of the auth flow.
    Api,
    /// WebSocket traffic.
    WebSocket,
    /// Call sites without a more specific route class.
    Other,
}

impl fmt::Display for ClientRouteClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auth => "auth",
            Self::Api => "api",
            Self::WebSocket => "wss",
            Self::Other => "other",
        })
    }
}

/// Coarse failure class for route selection errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteFailureClass {
    ProxyResolutionUnavailable,
    ConnectTimeout,
    ProxyAuthenticationRequired,
    TlsError,
    InvalidProxyConfig,
    UnsupportedProxyScheme,
    ResolverError,
}

impl fmt::Display for RouteFailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ProxyResolutionUnavailable => "proxy_resolution_unavailable",
            Self::ConnectTimeout => "connect_timeout",
            Self::ProxyAuthenticationRequired => "proxy_407",
            Self::TlsError => "tls_error",
            Self::InvalidProxyConfig => "invalid_proxy_config",
            Self::UnsupportedProxyScheme => "unsupported_proxy_scheme",
            Self::ResolverError => "resolver_error",
        })
    }
}

/// Resolved outbound proxy behavior for HTTP clients.
///
/// Callers must choose a policy explicitly so omitting feature resolution cannot silently select
/// legacy behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundProxyPolicy {
    /// Preserve reqwest's built-in proxy behavior.
    ReqwestDefault,
    /// Resolve system/PAC/WPAD settings, then environment settings, then direct routing.
    RespectSystemProxy,
}

/// Resolved proxy route for a concrete outbound destination.
///
/// `TransportDefault` preserves the underlying transport behavior only when system-proxy support
/// is disabled. When system resolution is enabled, environment and direct fallbacks are resolved
/// explicitly so the transport cannot repeat system discovery. Proxy URLs and no-proxy settings
/// are intentionally redacted from `Debug` output because they may contain credentials or private
/// hostnames.
#[derive(Clone, Hash, PartialEq, Eq)]
pub enum OutboundProxyRoute {
    /// Preserve the underlying transport's existing proxy behavior.
    TransportDefault,
    /// Connect directly and bypass transport-level proxy discovery.
    Direct,
    /// Connect through the selected proxy URL.
    Proxy {
        url: String,
        no_proxy: Option<String>,
    },
    /// Try PAC candidates in order, advancing only before application dispatch.
    /// Candidates are flat direct/proxy routes with destination bypass already
    /// resolved; proxy candidates do not carry an additional `no_proxy` policy.
    Fallback { routes: Vec<OutboundProxyRoute> },
}

impl fmt::Debug for OutboundProxyRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransportDefault => f.write_str("TransportDefault"),
            Self::Direct => f.write_str("Direct"),
            Self::Fallback { routes } => f.debug_struct("Fallback")
                .field("candidate_count", &routes.len()).finish(),
            Self::Proxy { .. } => f
                .debug_struct("Proxy")
                .field("url", &"<redacted>")
                .field("no_proxy", &"<redacted>")
                .finish(),
        }
    }
}

/// Builds route-specific HTTP clients using one resolved outbound proxy policy.
///
/// Construct this once from the effective application configuration and carry it with the
/// session or component that owns outbound requests. Individual request paths should supply only
/// their destination and route class rather than resolving feature state themselves.
#[derive(Clone)]
pub struct HttpClientFactory {
    outbound_proxy_policy: OutboundProxyPolicy,
    chatgpt_cookie_store: Option<Arc<ChatGptCookieStore>>,
}

impl PartialEq for HttpClientFactory {
    fn eq(&self, other: &Self) -> bool {
        self.outbound_proxy_policy == other.outbound_proxy_policy
            && self
                .chatgpt_cookie_store
                .as_ref()
                .map(|store| store.configured_cookies())
                == other
                    .chatgpt_cookie_store
                    .as_ref()
                    .map(|store| store.configured_cookies())
    }
}

impl Eq for HttpClientFactory {}

impl fmt::Debug for HttpClientFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClientFactory")
            .field("outbound_proxy_policy", &self.outbound_proxy_policy)
            .finish()
    }
}

impl HttpClientFactory {
    /// Creates a factory from the outbound proxy policy resolved by the application.
    pub const fn new(outbound_proxy_policy: OutboundProxyPolicy) -> Self {
        Self {
            outbound_proxy_policy,
            chatgpt_cookie_store: None,
        }
    }

    /// Adds process-scoped cookies to requests made by ChatGPT cookie-store clients.
    pub fn with_chatgpt_cookies(mut self, cookies: impl IntoIterator<Item = HeaderValue>) -> Self {
        let cookies = cookies.into_iter().collect::<Vec<_>>();
        self.chatgpt_cookie_store =
            (!cookies.is_empty()).then(|| Arc::new(ChatGptCookieStore::new(cookies)));
        self
    }

    /// Returns whether ChatGPT cookie-store clients have additional configured cookies.
    pub fn has_chatgpt_cookies(&self) -> bool {
        self.chatgpt_cookie_store.is_some()
    }

    pub(crate) fn chatgpt_cookie_store(&self) -> Option<Arc<ChatGptCookieStore>> {
        self.chatgpt_cookie_store.clone()
    }

    /// Returns the outbound proxy policy used for clients built by this factory.
    pub const fn outbound_proxy_policy(&self) -> OutboundProxyPolicy {
        self.outbound_proxy_policy
    }

    /// Resolves the proxy route for a concrete destination.
    ///
    /// WebSocket schemes are resolved through their HTTP equivalents so platform PAC and system
    /// proxy APIs apply the same policy to `ws`/`wss` and `http`/`https` destinations. When system
    /// resolution is unavailable, explicit environment settings are resolved before falling back
    /// to a direct route.
    pub fn resolve_proxy_route(&self, request_url: &str) -> OutboundProxyRoute {
        resolve_proxy_route(
            &ProcessEnv,
            request_url,
            self.outbound_proxy_policy,
            resolve_system_proxy,
        )
    }

    /// Resolves the proxy route for a concrete destination without blocking a Tokio worker.
    pub async fn resolve_proxy_route_async(
        &self,
        request_url: String,
    ) -> io::Result<OutboundProxyRoute> {
        if matches!(
            self.outbound_proxy_policy,
            OutboundProxyPolicy::ReqwestDefault
        ) {
            return Ok(OutboundProxyRoute::TransportDefault);
        }

        if let Some(route) = self.cached_proxy_route(&request_url) {
            return Ok(route);
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        return Ok(self.resolve_proxy_route(&request_url));

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let permit = ASYNC_SYSTEM_PROXY_RESOLUTION_PERMIT
                .acquire()
                .await
                .map_err(io::Error::other)?;
            if let Some(route) = self.cached_proxy_route(&request_url) {
                return Ok(route);
            }
            let factory = self.clone();
            tokio::task::spawn_blocking(move || {
                // Keep the permit with the blocking task: cancelling the caller must not allow a
                // second PAC/WinHTTP lookup to start while this one is still running.
                let _permit = permit;
                factory.resolve_proxy_route(&request_url)
            })
            .await
            .map_err(io::Error::other)
        }
    }

    fn cached_proxy_route(&self, request_url: &str) -> Option<OutboundProxyRoute> {
        let env_proxy_kind = EnvProxyKind::from_request_url(request_url);
        let request_url = proxy_resolution_url(request_url);
        if RequestOrigin::parse(&request_url).is_none() {
            return Some(OutboundProxyRoute::Direct);
        }
        cached_system_proxy_decision(&request_url)
            .map(|decision| route_from_system_decision(&ProcessEnv, env_proxy_kind, decision))
    }

    /// Discard a failed pre-dispatch route so the next lookup consults the OS.
    /// This never authorizes replay after a request may have reached its peer.
    pub fn invalidate_cached_proxy_route(&self, request_url: &str) {
        if self.outbound_proxy_policy != OutboundProxyPolicy::RespectSystemProxy {
            return;
        }
        if let Some(cache) = SYSTEM_PROXY_CACHE.get() {
            let key = system_proxy_cache_key(&proxy_resolution_url(request_url));
            let mut cache = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Fence an older in-flight PAC lookup from republishing its result
            // after this invalidation. Epoch publication shares the cache lock.
            SYSTEM_PROXY_CACHE_GENERATION.fetch_add(1, Ordering::AcqRel);
            cache.remove(&key);
        }
    }

    /// Builds a reqwest client for a concrete outbound route.
    ///
    /// Ordered PAC plans require [`Self::build_client`] or [`crate::RouteAwareClientPool`].
    /// A raw reqwest client cannot implement safe ordered connection fallback.
    pub fn build_reqwest_client(
        &self,
        builder: reqwest::ClientBuilder,
        request_url: &str,
        route_class: ClientRouteClass,
    ) -> Result<reqwest::Client, BuildRouteAwareHttpClientError> {
        build_reqwest_client_for_route(
            builder,
            request_url,
            route_class,
            self.outbound_proxy_policy,
            CustomCaPolicy::HonorProcessEnvironment,
        )
    }

    pub(crate) fn build_reqwest_client_for_resolved_route(
        &self,
        builder: reqwest::ClientBuilder,
        route_class: ClientRouteClass,
        route: &OutboundProxyRoute,
        custom_ca_policy: CustomCaPolicy,
    ) -> Result<reqwest::Client, BuildRouteAwareHttpClientError> {
        let builder = configure_builder_for_resolved_route(builder, route_class, route)?;
        build_reqwest_client_with_custom_ca_policy(builder, custom_ca_policy).map_err(Into::into)
    }
}

fn resolve_proxy_route(
    env: &dyn EnvSource,
    request_url: &str,
    outbound_proxy_policy: OutboundProxyPolicy,
    resolve_system_proxy: impl FnOnce(&str, &RequestOrigin) -> SystemProxyDecision,
) -> OutboundProxyRoute {
    if matches!(outbound_proxy_policy, OutboundProxyPolicy::ReqwestDefault) {
        return OutboundProxyRoute::TransportDefault;
    }

    let env_proxy_kind = EnvProxyKind::from_request_url(request_url);
    let request_url = proxy_resolution_url(request_url);
    let Some(origin) = RequestOrigin::parse(&request_url) else {
        return OutboundProxyRoute::Direct;
    };

    route_from_system_decision(
        env,
        env_proxy_kind,
        resolve_system_proxy(&request_url, &origin),
    )
}

fn route_from_system_decision(
    env: &dyn EnvSource,
    env_proxy_kind: EnvProxyKind,
    decision: SystemProxyDecision,
) -> OutboundProxyRoute {
    match decision {
        SystemProxyDecision::Direct => OutboundProxyRoute::Direct,
        SystemProxyDecision::Proxy { url } => OutboundProxyRoute::Proxy {
            url,
            no_proxy: None,
        },
        SystemProxyDecision::Unavailable { .. } => resolve_env_proxy_route(env, env_proxy_kind),
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        SystemProxyDecision::Fallback { routes } => OutboundProxyRoute::Fallback { routes },
    }
}

fn resolve_env_proxy_route(
    env: &dyn EnvSource,
    env_proxy_kind: EnvProxyKind,
) -> OutboundProxyRoute {
    let proxy_url = match env_proxy_kind {
        EnvProxyKind::Https => {
            proxy_env_value(env, "HTTPS_PROXY").or_else(|| proxy_env_value(env, "ALL_PROXY"))
        }
        EnvProxyKind::SecureWebSocket => proxy_env_value(env, "HTTPS_PROXY")
            .or_else(|| proxy_env_value(env, "HTTP_PROXY"))
            .or_else(|| proxy_env_value(env, "ALL_PROXY")),
        EnvProxyKind::Http => {
            proxy_env_value(env, "HTTP_PROXY").or_else(|| proxy_env_value(env, "ALL_PROXY"))
        }
        EnvProxyKind::Other => proxy_env_value(env, "ALL_PROXY"),
    };
    match proxy_url {
        Some(url) => OutboundProxyRoute::Proxy {
            url,
            no_proxy: proxy_env_value(env, "NO_PROXY"),
        },
        None => OutboundProxyRoute::Direct,
    }
}

/// Environment route for WebSockets, shared by default and system-resolver fallback.
pub fn websocket_environment_proxy_route(request_url: &str) -> OutboundProxyRoute {
    resolve_env_proxy_route(&ProcessEnv, EnvProxyKind::from_request_url(request_url))
}

/// Destination bypass policy shared by HTTP system proxies and WebSockets.
pub fn proxy_bypasses_url(no_proxy: &str, request_url: &str) -> bool {
    RequestOrigin::parse(request_url)
        .is_some_and(|origin| no_proxy_matches_origin(no_proxy, &origin))
}

#[derive(Clone, Copy)]
enum EnvProxyKind {
    Http,
    Https,
    SecureWebSocket,
    Other,
}

impl EnvProxyKind {
    fn from_request_url(request_url: &str) -> Self {
        let scheme = request_url
            .parse::<http::Uri>()
            .ok()
            .and_then(|uri| uri.scheme_str().map(str::to_ascii_lowercase));
        match scheme.as_deref() {
            Some("http" | "ws") => Self::Http,
            Some("https") => Self::Https,
            Some("wss") => Self::SecureWebSocket,
            Some(_) | None => Self::Other,
        }
    }
}

fn proxy_resolution_url(request_url: &str) -> Cow<'_, str> {
    if let Some(suffix) = request_url.strip_prefix("wss://") {
        Cow::Owned(format!("https://{suffix}"))
    } else if let Some(suffix) = request_url.strip_prefix("ws://") {
        Cow::Owned(format!("http://{suffix}"))
    } else {
        Cow::Borrowed(request_url)
    }
}

/// Error while building a resolver-aware reqwest client.
#[derive(Debug, Error)]
pub enum BuildRouteAwareHttpClientError {
    #[error(transparent)]
    CustomCa(#[from] BuildCustomCaTransportError),

    #[error("Failed to configure outbound proxy selected for {route_class}")]
    InvalidProxyConfig { route_class: ClientRouteClass },
    #[error("Ordered PAC routes require the HTTP client facade, not a raw reqwest client")]
    OrderedProxyRoutesRequireFacade,
}

impl From<BuildRouteAwareHttpClientError> for io::Error {
    fn from(error: BuildRouteAwareHttpClientError) -> Self {
        match error {
            BuildRouteAwareHttpClientError::CustomCa(error) => error.into(),
            BuildRouteAwareHttpClientError::InvalidProxyConfig { .. }
            | BuildRouteAwareHttpClientError::OrderedProxyRoutesRequireFacade => io::Error::other(error),
        }
    }
}

/// Builds a reqwest client with conservative route selection and shared CA handling.
///
/// Unavailable platform resolution falls back to environment proxies and then direct. Errors after
/// a route is selected are returned without trying another route. Ordered PAC candidates require
/// the HTTP facade, which distinguishes pre-dispatch failure from a possibly accepted request.
fn build_reqwest_client_for_route(
    builder: reqwest::ClientBuilder,
    request_url: &str,
    route_class: ClientRouteClass,
    outbound_proxy_policy: OutboundProxyPolicy,
    custom_ca_policy: CustomCaPolicy,
) -> Result<reqwest::Client, BuildRouteAwareHttpClientError> {
    let builder = configure_proxy_for_route(
        &ProcessEnv,
        builder,
        request_url,
        route_class,
        outbound_proxy_policy,
        resolve_system_proxy,
    )?;
    build_reqwest_client_with_custom_ca_policy(builder, custom_ca_policy).map_err(Into::into)
}

fn configure_proxy_for_route(
    env: &dyn EnvSource,
    builder: reqwest::ClientBuilder,
    request_url: &str,
    route_class: ClientRouteClass,
    outbound_proxy_policy: OutboundProxyPolicy,
    resolve_system_proxy: impl FnOnce(&str, &RequestOrigin) -> SystemProxyDecision,
) -> Result<reqwest::ClientBuilder, BuildRouteAwareHttpClientError> {
    let route = resolve_proxy_route(
        env,
        request_url,
        outbound_proxy_policy,
        resolve_system_proxy,
    );
    configure_builder_for_resolved_route(builder, route_class, &route)
}

fn configure_builder_for_resolved_route(
    builder: reqwest::ClientBuilder,
    route_class: ClientRouteClass,
    route: &OutboundProxyRoute,
) -> Result<reqwest::ClientBuilder, BuildRouteAwareHttpClientError> {
    match route {
        OutboundProxyRoute::TransportDefault => Ok(builder),
        OutboundProxyRoute::Fallback { .. } => Err(BuildRouteAwareHttpClientError::OrderedProxyRoutesRequireFacade),
        OutboundProxyRoute::Direct => Ok(builder.no_proxy()),
        OutboundProxyRoute::Proxy { url, no_proxy } => {
            let no_proxy = no_proxy.as_deref().and_then(reqwest::NoProxy::from_string);
            configure_concrete_proxy(builder, route_class, url, no_proxy)
        }
    }
}

fn configure_concrete_proxy(
    builder: reqwest::ClientBuilder,
    route_class: ClientRouteClass,
    proxy_url: &str,
    no_proxy: Option<reqwest::NoProxy>,
) -> Result<reqwest::ClientBuilder, BuildRouteAwareHttpClientError> {
    let proxy = match reqwest::Proxy::all(proxy_url) {
        Ok(proxy) => proxy,
        Err(_source) => {
            return Err(BuildRouteAwareHttpClientError::InvalidProxyConfig { route_class });
        }
    };
    Ok(builder.no_proxy().proxy(proxy.no_proxy(no_proxy)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
struct RequestOrigin {
    scheme: String,
    host: String,
    port: u16,
}

impl RequestOrigin {
    fn parse(request_url: &str) -> Option<Self> {
        let uri = request_url.parse::<http::Uri>().ok()?;
        let scheme = uri.scheme_str()?.to_ascii_lowercase();
        let host = uri.host()?.trim_matches(['[', ']']).to_ascii_lowercase();
        let port = uri.port_u16().or(match scheme.as_str() {
            "http" | "ws" => Some(80),
            "https" | "wss" => Some(443),
            _ => None,
        })?;
        Some(Self { scheme, host, port })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SystemProxyDecision {
    Direct,
    Proxy { url: String },
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    Fallback { routes: Vec<OutboundProxyRoute> },
    Unavailable { failure: RouteFailureClass },
}

fn resolve_system_proxy(request_url: &str, origin: &RequestOrigin) -> SystemProxyDecision {
    let cache = SYSTEM_PROXY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    resolve_system_proxy_with(
        cache,
        &SYSTEM_PROXY_RESOLUTION_LOCK,
        request_url,
        origin,
        resolve_platform_system_proxy,
    )
}

fn resolve_system_proxy_with(
    cache: &Mutex<HashMap<[u8; 32], CachedSystemProxyDecision>>,
    resolution_lock: &Mutex<()>,
    request_url: &str,
    origin: &RequestOrigin,
    resolve_platform_system_proxy: impl FnOnce(&str, &RequestOrigin) -> SystemProxyDecision,
) -> SystemProxyDecision {
    let cache_key = system_proxy_cache_key(request_url);
    {
        // Entries are self-contained; interrupted eviction or insertion leaves the
        // remaining decisions valid, so poisoning need not disable proxy routing.
        let mut cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(decision) =
            cached_system_proxy_decision_from_cache(&mut cache, &cache_key, Instant::now())
        {
            return decision;
        }
    }

    // Serialize misses, not cache hits. Keep ownership through publication, and recheck
    // after waiting so competing callers still share a single platform lookup.
    // This lock carries no state: a panicking resolver can safely be retried.
    let _resolution = resolution_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut decisions = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(decision) =
        cached_system_proxy_decision_from_cache(&mut decisions, &cache_key, Instant::now())
    {
        return decision;
    }
    let generation = SYSTEM_PROXY_CACHE_GENERATION.load(Ordering::Acquire);
    drop(decisions);

    // PAC/WPAD can take seconds. Do not hold the cache lock across platform I/O:
    // unrelated HTTP requests and WebSocket reconnects may already have cached routes.
    let decision = resolve_platform_system_proxy(request_url, origin);
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if generation == SYSTEM_PROXY_CACHE_GENERATION.load(Ordering::Acquire) {
        insert_system_proxy_cache_entry(&mut cache, &cache_key, decision.clone(), Instant::now());
    }
    decision
}

#[cfg(target_os = "macos")]
fn resolve_platform_system_proxy(request_url: &str, origin: &RequestOrigin) -> SystemProxyDecision {
    macos::resolve(request_url, origin)
}

#[cfg(target_os = "windows")]
fn resolve_platform_system_proxy(request_url: &str, origin: &RequestOrigin) -> SystemProxyDecision {
    windows::resolve(request_url, origin)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn resolve_platform_system_proxy(
    _request_url: &str,
    _origin: &RequestOrigin,
) -> SystemProxyDecision {
    SystemProxyDecision::Unavailable {
        failure: RouteFailureClass::ProxyResolutionUnavailable,
    }
}

#[derive(Debug, Clone)]
struct CachedSystemProxyDecision {
    decision: SystemProxyDecision,
    expires_at: Instant,
    generation: u64,
}

static SYSTEM_PROXY_CACHE: OnceLock<Mutex<HashMap<[u8; 32], CachedSystemProxyDecision>>> =
    OnceLock::new();
static SYSTEM_PROXY_RESOLUTION_LOCK: Mutex<()> = Mutex::new(());
static SYSTEM_PROXY_CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

fn cached_system_proxy_decision(request_url: &str) -> Option<SystemProxyDecision> {
    let cache = SYSTEM_PROXY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = system_proxy_cache_key(request_url);
    // Only bounded in-memory work holds this lock. Waiting for a short cache mutation
    // avoids turning a warm hit into a miss queued behind a slow platform lookup.
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cached_system_proxy_decision_from_cache(&mut cache, &key, Instant::now())
}

fn cached_system_proxy_decision_from_cache(
    cache: &mut HashMap<[u8; 32], CachedSystemProxyDecision>,
    cache_key: &[u8; 32],
    now: Instant,
) -> Option<SystemProxyDecision> {
    let cached = cache.get(cache_key)?;
    if cached.expires_at > now
        && cached.generation == SYSTEM_PROXY_CACHE_GENERATION.load(Ordering::Acquire)
    {
        return Some(cached.decision.clone());
    }
    cache.remove(cache_key);
    None
}

fn cache_system_proxy_decision(request_url: &str, decision: SystemProxyDecision) {
    let cache = SYSTEM_PROXY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache_key = system_proxy_cache_key(request_url);
    insert_system_proxy_cache_entry(&mut cache, &cache_key, decision, Instant::now());
}


fn insert_system_proxy_cache_entry(
    cache: &mut HashMap<[u8; 32], CachedSystemProxyDecision>,
    cache_key: &[u8; 32],
    decision: SystemProxyDecision,
    now: Instant,
) {
    let ttl = match &decision {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        SystemProxyDecision::Fallback { .. } => SYSTEM_PROXY_SUCCESS_CACHE_TTL,
        SystemProxyDecision::Direct | SystemProxyDecision::Proxy { .. } => {
            SYSTEM_PROXY_SUCCESS_CACHE_TTL
        }
        SystemProxyDecision::Unavailable { .. } => SYSTEM_PROXY_UNAVAILABLE_CACHE_TTL,
    };

    let generation = SYSTEM_PROXY_CACHE_GENERATION.load(Ordering::Acquire);
    cache.retain(|_, cached| cached.expires_at > now && cached.generation == generation);
    if cache.len() >= SYSTEM_PROXY_CACHE_MAX_ENTRIES
        && !cache.contains_key(cache_key)
        && let Some(cache_key_to_evict) = cache
            .iter()
            .min_by_key(|(_, cached)| cached.expires_at)
            .map(|(cache_key, _)| *cache_key)
    {
        cache.remove(&cache_key_to_evict);
    }
    cache.insert(
        *cache_key,
        CachedSystemProxyDecision {
            decision,
            expires_at: now + ttl,
            generation,
        },
    );
}

fn system_proxy_cache_key(request_url: &str) -> [u8; 32] {
    // Keep URL-specific PAC decisions without retaining the raw routed URL.
    // The cache is process-local: hexadecimal encoding only adds an allocation
    // and doubles the hashed key bytes on every warm lookup.
    let mut hasher = Sha256::new();
    hasher.update(b"system-proxy-cache-v1\0");
    hasher.update(request_url.as_bytes());
    hasher.finalize().into()
}

fn no_proxy_matches_origin(no_proxy: &str, origin: &RequestOrigin) -> bool {
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| no_proxy_entry_matches_origin(entry, origin))
}

fn no_proxy_entry_matches_origin(entry: &str, origin: &RequestOrigin) -> bool {
    if entry == "*" {
        return true;
    }

    let mut entry = entry
        .strip_prefix("http://")
        .or_else(|| entry.strip_prefix("https://"))
        .unwrap_or(entry)
        .to_ascii_lowercase();
    if let Some((network, prefix)) = entry.rsplit_once('/') {
        let network = network.trim_matches(['[', ']']).parse::<std::net::IpAddr>();
        let address = origin.host.parse::<std::net::IpAddr>();
        let prefix = prefix.parse::<u32>();
        return match (network, address, prefix) {
            (Ok(std::net::IpAddr::V4(network)), Ok(std::net::IpAddr::V4(address)), Ok(bits @ 0..=32)) => {
                let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
                u32::from(network) & mask == u32::from(address) & mask
            }
            (Ok(std::net::IpAddr::V6(network)), Ok(std::net::IpAddr::V6(address)), Ok(bits @ 0..=128)) => {
                let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        };
    }
    let mut port = None;
    if let Some(bracketed) = entry.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        if !suffix.is_empty() {
            port = suffix
                .strip_prefix(':')
                .and_then(|port| port.parse::<u16>().ok());
            if port.is_none() {
                return false;
            }
        }
        entry = host.to_string();
    } else if let Some((host, parsed_port)) =
        entry.rsplit_once(':').and_then(|(host, candidate_port)| {
            if host.contains(':') {
                return None;
            }
            candidate_port
                .parse::<u16>()
                .ok()
                .map(|parsed_port| (host.to_string(), parsed_port))
        })
    {
        entry = host;
        port = Some(parsed_port);
    }
    if port.is_some_and(|port| port != origin.port) {
        return false;
    }

    if let Some(suffix) = entry.strip_prefix('.') {
        return origin.host == suffix || origin.host.ends_with(&format!(".{suffix}"));
    }

    if entry.contains('*') {
        return wildcard_host_match(&entry, &origin.host);
    }

    origin.host == entry
        || (origin.host.parse::<std::net::IpAddr>().is_err()
            && origin.host.ends_with(&format!(".{entry}")))
}

fn wildcard_host_match(pattern: &str, host: &str) -> bool {
    let mut remaining = host;
    let mut first = true;
    let mut parts = pattern.split('*').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(stripped) = remaining.strip_prefix(part) else {
                return false;
            };
            remaining = stripped;
        } else if parts.peek().is_none() {
            return remaining.ends_with(part);
        } else {
            let Some(index) = remaining.find(part) else {
                return false;
            };
            remaining = &remaining[index + part.len()..];
        }
        first = false;
    }
    pattern.ends_with('*') || remaining.is_empty()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedProxyListDecision {
    Direct,
    Proxy(String),
    UnsupportedScheme,
    Unavailable,
}

#[cfg(target_os = "windows")]
fn parse_static_proxy_list(input: &str, target_scheme: &str) -> ParsedProxyListDecision {
    let mut saw_mapping = false;
    let mut saw_default = false;
    for token in input
        .split(|ch: char| ch == ';' || ch.is_whitespace())
        .filter(|token| !token.is_empty())
    {
        if let Some((key, _)) = token.split_once('=') {
            saw_mapping = true;
            if key.eq_ignore_ascii_case(target_scheme) {
                return parse_proxy_token(token, target_scheme);
            }
        } else {
            saw_default = true;
        }
    }
    if saw_mapping && !saw_default {
        ParsedProxyListDecision::Direct
    } else {
        parse_proxy_list(input, target_scheme)
    }
}

fn parse_proxy_list(input: &str, target_scheme: &str) -> ParsedProxyListDecision {
    let mut saw_unsupported = false;
    for token in proxy_list_tokens(input) {
        match parse_proxy_token(token, target_scheme) {
            decision @ (ParsedProxyListDecision::Direct | ParsedProxyListDecision::Proxy(_)) => return decision,
            ParsedProxyListDecision::UnsupportedScheme => saw_unsupported = true,
            ParsedProxyListDecision::Unavailable => {},
        }
    }
    if saw_unsupported {
        ParsedProxyListDecision::UnsupportedScheme
    } else {
        ParsedProxyListDecision::Unavailable
    }
}

fn proxy_list_tokens(input: &str) -> impl Iterator<Item = &str> {
    input.split(';').map(str::trim).filter(|segment| !segment.is_empty()).flat_map(|segment| {
        let mut parts = segment.split_whitespace();
        let directive = parts.next();
        let hostport = parts.next();
        let extra = parts.next();
        let is_proxy_directive = matches!(
            directive.map(str::to_ascii_lowercase).as_deref(),
            Some("proxy" | "http" | "https" | "socks" | "socks4" | "socks5")
        ) && hostport.is_some() && extra.is_none();
        // Keep a PAC directive and its authority together. Bare WinHTTP proxy
        // lists may instead contain whitespace-separated authorities.
        let directive = is_proxy_directive.then_some(segment);
        let tokens = (!is_proxy_directive).then(|| segment.split_whitespace());
        directive.into_iter().chain(tokens.into_iter().flatten())
    })
}

fn parse_proxy_token(token: &str, target_scheme: &str) -> ParsedProxyListDecision {
    if token.eq_ignore_ascii_case("DIRECT") {
        return ParsedProxyListDecision::Direct;
    }

    if let Some(decision) = parse_proxy_key_token(token, target_scheme) {
        return decision;
    }
    if token.contains('=') {
        return ParsedProxyListDecision::Unavailable;
    }

    let mut parts = token.split_whitespace();
    let directive = parts.next();
    let hostport = parts.next();
    if let (Some(directive), Some(hostport), None) = (directive, hostport, parts.next()) {
        return match directive.to_ascii_lowercase().as_str() {
            "proxy" | "http" => proxy_url_from_hostport("http", hostport),
            "https" => proxy_url_from_hostport("https", hostport),
            "socks" | "socks4" | "socks5" => ParsedProxyListDecision::UnsupportedScheme,
            _ => ParsedProxyListDecision::Unavailable,
        };
    }

    proxy_url_from_hostport("http", token)
}

fn parse_proxy_key_token(token: &str, target_scheme: &str) -> Option<ParsedProxyListDecision> {
    let (key, value) = token.split_once('=')?;
    if key.trim().eq_ignore_ascii_case(target_scheme) {
        Some(proxy_url_from_hostport("http", value.trim()))
    } else {
        Some(ParsedProxyListDecision::Unavailable)
    }
}

fn proxy_url_from_hostport(proxy_scheme: &str, hostport: &str) -> ParsedProxyListDecision {
    if hostport.is_empty() {
        return ParsedProxyListDecision::Unavailable;
    }
    if hostport.contains("://") {
        return ParsedProxyListDecision::Proxy(hostport.to_string());
    }
    ParsedProxyListDecision::Proxy(format!("{proxy_scheme}://{hostport}"))
}

trait EnvSource {
    fn var(&self, key: &str) -> Option<String>;
}

struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

fn proxy_env_value(env: &dyn EnvSource, upper: &str) -> Option<String> {
    let lower = upper.to_ascii_lowercase();
    env.var(upper)
        .or_else(|| env.var(&lower))
        .filter(|value| !value.is_empty())
}



