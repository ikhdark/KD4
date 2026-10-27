//! Reusable HTTP client and request-builder wrappers.

use bytes::Bytes;
use futures::TryStream;
use http::Error as HttpRequestBuildError;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use opentelemetry::global;
use opentelemetry::propagation::Injector;
use reqwest::IntoUrl;
use reqwest::Method;
use serde::Serialize;
use std::cell::Cell;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub type HttpError = reqwest::Error;
pub type HttpResponse = reqwest::Response;

/// Reusable HTTP client wrapper with shared tracing and request-diagnostic behavior.
///
/// Product callers should obtain this through [`crate::HttpClientFactory`] for a fixed
/// destination or use [`crate::RouteAwareClientPool`] when request and redirect URLs can vary.
#[derive(Clone, Debug)]
pub struct HttpClient {
    inner: reqwest::Client,
    request_logging: RequestLogging,
    default_headers: Arc<HeaderMap>,
    fallback: Option<Arc<FallbackTransports>>,
}

impl HttpClient {
    pub fn new(inner: reqwest::Client) -> Self {
        Self::from_parts(inner, RequestLogging::Enabled)
    }

    /// Creates a client that suppresses request URL and response-header diagnostics.
    ///
    /// Use this for endpoints whose URLs or headers may contain credentials that are redacted by
    /// the caller above the HTTP transport boundary.
    pub fn new_without_request_logging(inner: reqwest::Client) -> Self {
        Self::from_parts(inner, RequestLogging::Disabled)
    }

    pub(crate) fn from_parts(inner: reqwest::Client, request_logging: RequestLogging) -> Self {
        Self {
            inner,
            request_logging,
            default_headers: Arc::default(),
            fallback: None,
        }
    }

    pub(crate) fn with_default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers = Arc::new(headers);
        self
    }

    pub(crate) fn with_fallbacks(
        mut self,
        primary_route: &crate::OutboundProxyRoute,
        clients: Vec<(HttpClient, &crate::OutboundProxyRoute)>,
        timeout: Option<Duration>,
    ) -> Self {
        self.fallback = Some(Arc::new(FallbackTransports {
            primary_proxy_authority: proxy_credential_authority(primary_route),
            clients: clients.into_iter().map(|(client, route)| FallbackTransport {
                client: client.inner,
                proxy_authority: proxy_credential_authority(route),
            }).collect(),
            timeout,
        }));
        self
    }


    pub fn get<U>(&self, url: U) -> RequestBuilder
    where
        U: IntoUrl,
    {
        self.request(Method::GET, url)
    }

    pub fn head<U>(&self, url: U) -> RequestBuilder
    where
        U: IntoUrl,
    {
        self.request(Method::HEAD, url)
    }

    pub fn post<U>(&self, url: U) -> RequestBuilder
    where
        U: IntoUrl,
    {
        self.request(Method::POST, url)
    }

    pub fn delete<U>(&self, url: U) -> RequestBuilder
    where
        U: IntoUrl,
    {
        self.request(Method::DELETE, url)
    }

    pub fn request<U>(&self, method: Method, url: U) -> RequestBuilder
    where
        U: IntoUrl,
    {
        let diagnostic_url = self
            .request_logging_enabled()
            .then(|| url.as_str().to_string());
        RequestBuilder::new(
            self.inner.request(method.clone(), url),
            method,
            diagnostic_url,
            self.request_logging,
            Arc::clone(&self.default_headers),
            self.fallback.clone(),
        )
    }

    pub(crate) async fn execute(
        &self,
        mut request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        apply_default_headers(request.headers_mut(), &self.default_headers);
        let method = request.method().clone();
        let diagnostic_url = self
            .request_logging_enabled()
            .then(|| request.url().to_string());

        match self.execute_without_request_logging(request).await {
            Ok(response) => {
                if let Some(url) = diagnostic_url.as_deref() {
                    self.log_response(&method, url, &response);
                }
                Ok(response)
            }
            Err(error) => {
                if let Some(url) = diagnostic_url.as_deref() {
                    self.log_error(&method, url, &error);
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn execute_without_request_logging(
        &self,
        mut request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        request.headers_mut().extend(trace_headers());
        crate::transport::record_send_started();
        let result = execute_with_fallbacks(&self.inner, request, self.fallback.as_deref()).await;
        if result.is_ok() {
            crate::transport::record_headers_received();
        }
        result
    }

    pub(crate) fn log_response(&self, method: &Method, url: &str, response: &reqwest::Response) {
        if self.request_logging == RequestLogging::Enabled {
            tracing::debug!(
                method = %method,
                url = %url,
                status = %response.status(),
                headers = ?diagnostic_response_headers(response.headers()),
                version = ?response.version(),
                "Request completed"
            );
        }
    }

    pub(crate) fn log_error(&self, method: &Method, url: &str, error: &reqwest::Error) {
        if self.request_logging == RequestLogging::Enabled {
            tracing::debug!(
                method = %method,
                url = %url,
                status = error.status().map(|status| status.as_u16()),
                error = %error,
                "Request failed"
            );
        }
    }
    pub(crate) fn log_error_summary(&self, method: &Method, url: &str, error: &reqwest::Error) {
        if self.request_logging == RequestLogging::Enabled {
            tracing::debug!(
                method = %method,
                url = %url,
                status = error.status().map(|status| status.as_u16()),
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                "Request failed"
            );
        }
    }

    pub(crate) const fn request_logging_enabled(&self) -> bool {
        matches!(self.request_logging, RequestLogging::Enabled)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RequestLogging {
    #[default]
    Enabled,
    Disabled,
}

#[must_use = "requests are not sent unless `send` is awaited"]
pub struct RequestBuilder {
    builder: reqwest::RequestBuilder,
    method: Method,
    diagnostic_url: Option<String>,
    request_logging: RequestLogging,
    default_headers: Arc<HeaderMap>,
    fallback: Option<Arc<FallbackTransports>>,
}

impl std::fmt::Debug for RequestBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestBuilder")
            .field("method", &self.method)
            .field("url", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl RequestBuilder {
    fn new(
        builder: reqwest::RequestBuilder,
        method: Method,
        diagnostic_url: Option<String>,
        request_logging: RequestLogging,
        default_headers: Arc<HeaderMap>,
        fallback: Option<Arc<FallbackTransports>>,
    ) -> Self {
        Self {
            builder,
            method,
            diagnostic_url,
            request_logging,
            default_headers,
            fallback,
        }
    }

    fn map(self, f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Self {
        Self {
            builder: f(self.builder),
            method: self.method,
            diagnostic_url: self.diagnostic_url,
            request_logging: self.request_logging,
            default_headers: self.default_headers,
            fallback: self.fallback,
        }
    }

    pub fn headers(self, headers: HeaderMap) -> Self {
        self.map(|builder| builder.headers(headers))
    }

    pub fn header<K, V>(self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpRequestBuildError>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpRequestBuildError>,
    {
        self.map(|builder| builder.header(key, value))
    }

    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: Display,
    {
        self.map(|builder| builder.bearer_auth(token))
    }

    pub fn timeout(self, timeout: Duration) -> Self {
        self.map(|builder| builder.timeout(timeout))
    }

    pub fn json<T>(self, value: &T) -> Self
    where
        T: ?Sized + Serialize,
    {
        self.map(|builder| builder.json(value))
    }

    pub fn query<T>(self, query: &T) -> Self
    where
        T: ?Sized + Serialize,
    {
        self.map(|builder| builder.query(query))
    }

    pub fn body<B>(self, body: B) -> Self
    where
        B: Into<reqwest::Body>,
    {
        self.map(|builder| builder.body(body))
    }

    /// Sets a streaming request body without exposing the underlying HTTP implementation.
    pub fn body_stream<S>(self, stream: S) -> Self
    where
        S: TryStream + Send + 'static,
        S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        Bytes: From<S::Ok>,
    {
        self.map(|builder| builder.body(reqwest::Body::wrap_stream(stream)))
    }

    pub async fn send(self) -> Result<HttpResponse, HttpError> {
        let headers = trace_headers();

        let (client, request) = self.builder.headers(headers).build_split();
        let result = match request {
            Ok(mut request) => {
                apply_default_headers(request.headers_mut(), &self.default_headers);
                crate::transport::record_send_started();
                let result = execute_with_fallbacks(&client, request, self.fallback.as_deref()).await;
                if result.is_ok() {
                    crate::transport::record_headers_received();
                }
                result
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(response) => {
                if let (RequestLogging::Enabled, Some(url)) =
                    (self.request_logging, self.diagnostic_url.as_deref())
                {
                    tracing::debug!(
                        method = %self.method,
                        url = %url,
                        status = %response.status(),
                        headers = ?diagnostic_response_headers(response.headers()),
                        version = ?response.version(),
                        "Request completed"
                    );
                }

                Ok(response)
            }
            Err(error) => {
                if let (RequestLogging::Enabled, Some(url)) =
                    (self.request_logging, self.diagnostic_url.as_deref())
                {
                    let status = error.status();
                    tracing::debug!(
                        method = %self.method,
                        url = %url,
                        status = status.map(|s| s.as_u16()),
                        error = %error,
                        "Request failed"
                    );
                }
                Err(error)
            }
        }
    }
}

struct FallbackTransports {
    primary_proxy_authority: Option<ProxyCredentialAuthority>,
    clients: Vec<FallbackTransport>,
    timeout: Option<Duration>,
}

struct FallbackTransport {
    client: reqwest::Client,
    proxy_authority: Option<ProxyCredentialAuthority>,
}

// Include the proxy transport scheme so an HTTPS-to-HTTP downgrade never
// inherits credentials merely because its host/port happens to match.
type ProxyCredentialAuthority = (String, String, u16);

fn proxy_credential_authority(route: &crate::OutboundProxyRoute) -> Option<ProxyCredentialAuthority> {
    let crate::OutboundProxyRoute::Proxy { url, no_proxy: None } = route else { return None };
    let url = reqwest::Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https") { return None; }
    Some((url.scheme().to_string(), url.host_str()?.to_ascii_lowercase(), url.port_or_known_default()?))
}

impl std::fmt::Debug for FallbackTransports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackTransports").field("fallback_count", &self.clients.len()).finish()
    }
}

tokio::task_local! {
    // Reqwest evaluates its redirect policy while polling the execute future.
    // A scoped marker isolates concurrent requests without a client-global flag.
    static FALLBACK_REDIRECT_SEEN: Cell<bool>;
}

pub(crate) fn record_fallback_redirect() {
    let _ = FALLBACK_REDIRECT_SEEN.try_with(|seen| seen.set(true));
}

async fn execute_with_fallbacks(
    primary: &reqwest::Client,
    mut request: reqwest::Request,
    fallback: Option<&FallbackTransports>,
) -> Result<HttpResponse, HttpError> {
    let Some(fallback) = fallback else { return primary.execute(request).await };
    let deadline = request.timeout().copied().or(fallback.timeout)
        .map(|timeout| tokio::time::Instant::now() + timeout);
    // Own the current cheap client handle instead of retaining a chained iterator
    // of differently borrowed clients across the Send transport future.
    let mut client = primary.clone();
    // Bind only the original caller-supplied values to the original proxy.
    // A later client may inject its own credential, which must never become
    // provenance for another attempt (including a return to the first proxy).
    let credential_authority = fallback.primary_proxy_authority.as_ref();
    let caller_proxy_authorization = request.headers()
        .get_all(http::header::PROXY_AUTHORIZATION)
        .iter().cloned().collect::<Vec<_>>();
    let mut next_candidate = 0;
    loop {
        if let Some(deadline) = deadline {
            *request.timeout_mut() = Some(deadline.saturating_duration_since(tokio::time::Instant::now()));
        }
        let replay = request.try_clone();
        let (result, redirected) = FALLBACK_REDIRECT_SEEN.scope(Cell::new(false), async move {
            let result = client.execute(request).await;
            (result, FALLBACK_REDIRECT_SEEN.with(Cell::get))
        }).await;
        match result {
            Err(error) if error.is_connect()
                && !crate::is_permanent_connection_error(&error)
                && !redirected
                && next_candidate < fallback.clients.len() => {
                let Some(mut replay) = replay else { return Err(error) };
                let next = &fallback.clients[next_candidate];
                // Restore from the original binding, not the preceding replay:
                // A,B,A must authenticate A without disclosing A's header to B.
                replay.headers_mut().remove(http::header::PROXY_AUTHORIZATION);
                if credential_authority.zip(next.proxy_authority.as_ref())
                    .is_some_and(|(original, next)| original == next)
                {
                    for value in &caller_proxy_authorization {
                        replay.headers_mut().append(http::header::PROXY_AUTHORIZATION, value.clone());
                    }
                }
                request = replay;
                client = next.client.clone();
                next_candidate += 1;
            }
            result => return result,
        }
    }
}

fn diagnostic_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut redacted = HeaderMap::new();
    for (name, value) in headers {
        if matches!(
            name.as_str(),
            "content-type" | "content-length" | "x-request-id" | "request-id"
        ) && value.as_bytes().len() <= 256
        {
            redacted.append(name, value.clone());
        }
    }
    redacted
}

pub(crate) fn apply_default_headers(headers: &mut HeaderMap, defaults: &HeaderMap) {
    for name in defaults.keys() {
        if !headers.contains_key(name) {
            for value in defaults.get_all(name) {
                headers.append(name, value.clone());
            }
        }
    }
}

struct HeaderMapInjector<'a>(&'a mut HeaderMap);

impl<'a> Injector for HeaderMapInjector<'a> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}

pub(crate) fn trace_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    global::get_text_map_propagator(|prop| {
        prop.inject_context(
            &Span::current().context(),
            &mut HeaderMapInjector(&mut headers),
        );
    });
    headers
}

