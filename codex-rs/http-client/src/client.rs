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
        }
    }

    pub(crate) fn with_default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers = Arc::new(headers);
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
        let result = self.inner.execute(request).await;
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
    ) -> Self {
        Self {
            builder,
            method,
            diagnostic_url,
            request_logging,
            default_headers,
        }
    }

    fn map(self, f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Self {
        Self {
            builder: f(self.builder),
            method: self.method,
            diagnostic_url: self.diagnostic_url,
            request_logging: self.request_logging,
            default_headers: self.default_headers,
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
                let result = client.execute(request).await;
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

