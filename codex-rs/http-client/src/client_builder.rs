//! HTTP client construction that makes outbound proxy policy explicit.
//!
//! Product traffic should normally enter through [`HttpClientFactory`] for a fixed destination or
//! [`crate::RouteAwareClientPool`] when request and redirect URLs can vary. The remaining direct
//! and transport-default terminals exist only for narrow exceptional or compatibility paths.

use http::HeaderMap;
use std::sync::Arc;
use std::time::Duration;

use crate::BuildCustomCaTransportError;
use crate::BuildRouteAwareHttpClientError;
use crate::ClientRouteClass;
use crate::HttpClient;
use crate::HttpClientFactory;
use crate::OutboundProxyRoute;
use crate::chatgpt_cloudflare_cookies::ChatGptCookieStore;
use crate::client::RequestLogging;
use crate::custom_ca::build_reqwest_client_with_custom_ca;
use crate::with_chatgpt_cloudflare_cookie_store;

/// Configures an [`HttpClient`] without exposing the underlying HTTP implementation.
///
/// Product traffic should prefer [`HttpClientFactory::build_client`] or finish this builder with
/// [`Self::build_respecting_outbound_proxy_policy`]. The remaining terminal methods deliberately
/// bypass the factory and are restricted to documented exceptional or compatibility paths.
#[derive(Clone)]
pub struct HttpClientBuilder {
    default_headers: Option<HeaderMap>,
    follow_redirects: bool,
    connect_timeout: Option<Duration>,
    request_timeout: Option<Option<Duration>>,
    tls_certs_only: Option<Vec<reqwest::Certificate>>,
    identity: Option<reqwest::Identity>,
    https_only: bool,
    chatgpt_cloudflare_cookie_store: bool,
    chatgpt_cookie_store: Option<Arc<ChatGptCookieStore>>,
    request_logging: RequestLogging,
}

impl HttpClientFactory {
    /// Builds an HTTP client for one fixed destination using the configured proxy policy.
    ///
    /// This is the preferred construction path for product traffic that uses a fixed destination.
    /// Use [`crate::RouteAwareClientPool`] instead when request or redirect URLs can vary.
    pub fn build_client(
        &self,
        request_url: &str,
        route_class: ClientRouteClass,
    ) -> Result<HttpClient, BuildRouteAwareHttpClientError> {
        HttpClientBuilder::new().build_respecting_outbound_proxy_policy(
            self,
            request_url,
            route_class,
        )
    }

    /// Builds a policy-aware client without request URL or response-header diagnostics.
    ///
    /// This has the same routing guidance as [`Self::build_client`].
    pub fn build_client_without_request_logging(
        &self,
        request_url: &str,
        route_class: ClientRouteClass,
    ) -> Result<HttpClient, BuildRouteAwareHttpClientError> {
        HttpClientBuilder::new()
            .without_request_logging()
            .build_respecting_outbound_proxy_policy(self, request_url, route_class)
    }
}

impl HttpClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers = Some(headers);
        self
    }

    pub fn without_redirects(mut self) -> Self {
        self.follow_redirects = false;
        self
    }

    pub(crate) fn follows_redirects(&self) -> bool {
        self.follow_redirects
    }

    /// Limits only connection establishment, not the request as a whole.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Sets the client-wide request timeout. `None` disables the transport default timeout.
    pub fn request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    pub fn timeout(self, timeout: Duration) -> Self {
        self.request_timeout(Some(timeout))
    }

    /// Replaces the transport root set with the certificate encoded by `pem`.
    pub fn tls_certs_only_pem(mut self, pem: &[u8]) -> Result<Self, crate::HttpError> {
        let certificate = reqwest::Certificate::from_pem(pem)?;
        self.tls_certs_only = Some(vec![certificate]);
        Ok(self)
    }

    /// Configures a PEM-encoded client certificate and private key identity.
    pub fn identity_pem(mut self, pem: &[u8]) -> Result<Self, crate::HttpError> {
        self.identity = Some(reqwest::Identity::from_pem(pem)?);
        Ok(self)
    }

    pub fn https_only(mut self, enabled: bool) -> Self {
        self.https_only = enabled;
        self
    }

    pub fn with_chatgpt_cloudflare_cookie_store(mut self) -> Self {
        self.chatgpt_cloudflare_cookie_store = true;
        self
    }

    /// Uses the factory's configured ChatGPT cookies without changing proxy behavior.
    pub fn with_chatgpt_cookies(mut self, http_client_factory: &HttpClientFactory) -> Self {
        self.chatgpt_cloudflare_cookie_store = true;
        self.chatgpt_cookie_store = http_client_factory.chatgpt_cookie_store();
        self
    }

    /// Suppresses request URL and response-header diagnostics.
    pub fn without_request_logging(mut self) -> Self {
        self.request_logging = RequestLogging::Disabled;
        self
    }

    /// Builds a client that honors the [`HttpClientFactory`] outbound proxy policy.
    ///
    /// This is the preferred terminal method for product traffic. The request URL is used to
    /// resolve a concrete direct or proxy route when the factory is configured with
    /// [`crate::OutboundProxyPolicy::RespectSystemProxy`].
    pub fn build_respecting_outbound_proxy_policy(
        mut self,
        http_client_factory: &HttpClientFactory,
        request_url: &str,
        route_class: ClientRouteClass,
    ) -> Result<HttpClient, BuildRouteAwareHttpClientError> {
        self.chatgpt_cookie_store = http_client_factory.chatgpt_cookie_store();
        let (builder, request_logging) = self.into_reqwest_parts();
        let inner = http_client_factory.build_reqwest_client(builder, request_url, route_class)?;
        Ok(HttpClient::from_parts(inner, request_logging))
    }

    /// Builds a client for a route that was already resolved by a route-aware caller.
    pub(crate) fn build_for_resolved_route(
        mut self,
        http_client_factory: &HttpClientFactory,
        route_class: ClientRouteClass,
        route: &OutboundProxyRoute,
    ) -> Result<HttpClient, BuildRouteAwareHttpClientError> {
        self.chatgpt_cookie_store = http_client_factory.chatgpt_cookie_store();
        let (builder, request_logging) = self.into_reqwest_parts();
        let inner = http_client_factory.build_reqwest_client_for_resolved_route(
            builder,
            route_class,
            route,
        )?;
        Ok(HttpClient::from_parts(inner, request_logging))
    }

    /// Builds a client that connects directly without using a proxy.
    ///
    /// # Exceptional use only
    ///
    /// This bypasses [`HttpClientFactory`] and is appropriate only when bypassing proxy discovery
    /// is itself required: for example, a hermetic local test fixture, a localhost callback, or
    /// sandbox traffic whose egress routing is handled separately. Ordinary outbound product
    /// traffic must use [`Self::build_respecting_outbound_proxy_policy`] or
    /// [`HttpClientFactory::build_client`].
    pub fn build_direct(self) -> Result<HttpClient, BuildCustomCaTransportError> {
        let (builder, request_logging) = self.into_reqwest_parts();
        build_reqwest_client_with_custom_ca(builder.no_proxy())
            .map(|inner| HttpClient::from_parts(inner, request_logging))
    }

    /// Builds a client using the transport's default proxy behavior.
    ///
    /// # Compatibility boundary
    ///
    /// This preserves reqwest's built-in proxy selection for callers that have not opted into
    /// route-aware proxy resolution. Custom-CA and client-construction failures are returned to the
    /// caller; this method never retries with a different trust configuration.
    pub fn build_with_transport_default_proxy(
        self,
    ) -> Result<HttpClient, BuildCustomCaTransportError> {
        self.build_with_transport_default_proxy_using(build_reqwest_client_with_custom_ca)
    }

    fn build_with_transport_default_proxy_using(
        self,
        build_with_custom_ca: impl FnOnce(
            reqwest::ClientBuilder,
        )
            -> Result<reqwest::Client, BuildCustomCaTransportError>,
    ) -> Result<HttpClient, BuildCustomCaTransportError> {
        let request_logging = self.request_logging;
        build_with_custom_ca(self.base_reqwest_builder())
            .map(|inner| HttpClient::from_parts(inner, request_logging))
    }

    fn into_reqwest_parts(self) -> (reqwest::ClientBuilder, RequestLogging) {
        let request_logging = self.request_logging;
        (self.base_reqwest_builder(), request_logging)
    }

    fn base_reqwest_builder(self) -> reqwest::ClientBuilder {
        let mut builder = reqwest::Client::builder();
        if let Some(default_headers) = self.default_headers {
            builder = builder.default_headers(default_headers);
        }
        if !self.follow_redirects {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        if let Some(connect_timeout) = self.connect_timeout {
            builder = builder.connect_timeout(connect_timeout);
        }
        if let Some(Some(timeout)) = self.request_timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(certificates) = self.tls_certs_only {
            builder = builder.tls_certs_only(certificates);
        }
        if let Some(identity) = self.identity {
            builder = builder.identity(identity);
        }
        builder = builder.https_only(self.https_only);
        if self.chatgpt_cloudflare_cookie_store {
            builder = match self.chatgpt_cookie_store {
                Some(store) => builder.cookie_provider(store),
                None => with_chatgpt_cloudflare_cookie_store(builder),
            };
        }
        builder
    }
}

impl Default for HttpClientBuilder {
    fn default() -> Self {
        Self {
            default_headers: None,
            follow_redirects: true,
            connect_timeout: None,
            request_timeout: None,
            tls_certs_only: None,
            identity: None,
            https_only: false,
            chatgpt_cloudflare_cookie_store: false,
            chatgpt_cookie_store: None,
            request_logging: RequestLogging::Enabled,
        }
    }
}

#[cfg(test)]
#[path = "client_builder_tests.rs"]
mod tests;
