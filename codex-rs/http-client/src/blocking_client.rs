//! Blocking HTTP facade for synchronous product surfaces.

use std::io;
use std::io::Read;
use std::time::Duration;

use http::HeaderMap;
use http::Method;
use http::StatusCode;

use crate::BuildCustomCaTransportError;
use crate::HttpError;
use crate::custom_ca::CustomCaPolicy;
use crate::custom_ca::build_blocking_reqwest_client_with_custom_ca_policy;

/// Configures a blocking client without exposing the underlying transport.
pub struct BlockingHttpClientBuilder {
    follow_redirects: bool,
    request_timeout: Option<Option<Duration>>,
    tls_certs_only: Option<Vec<reqwest::Certificate>>,
    identity: Option<reqwest::Identity>,
    https_only: bool,
}

impl BlockingHttpClientBuilder {
    pub fn new() -> Self {
        Self {
            follow_redirects: true,
            request_timeout: None,
            tls_certs_only: None,
            identity: None,
            https_only: false,
        }
    }

    pub fn without_redirects(mut self) -> Self {
        self.follow_redirects = false;
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
    ///
    /// Process custom CA environment variables are not added to this exclusive root set.
    pub fn tls_certs_only_pem(mut self, pem: &[u8]) -> Result<Self, HttpError> {
        let certificate = reqwest::Certificate::from_pem(pem)?;
        self.tls_certs_only = Some(vec![certificate]);
        Ok(self)
    }

    /// Configures a PEM-encoded client certificate and private key identity.
    pub fn identity_pem(mut self, pem: &[u8]) -> Result<Self, HttpError> {
        self.identity = Some(reqwest::Identity::from_pem(pem)?);
        Ok(self)
    }

    pub fn https_only(mut self, enabled: bool) -> Self {
        self.https_only = enabled;
        self
    }

    /// Builds a client that never uses a proxy.
    pub fn build_direct(self) -> Result<BlockingHttpClient, BuildCustomCaTransportError> {
        self.build_inner(/*direct*/ true)
    }

    /// Builds a client using the transport's default proxy behavior.
    pub fn build_with_transport_default_proxy(
        self,
    ) -> Result<BlockingHttpClient, BuildCustomCaTransportError> {
        self.build_inner(/*direct*/ false)
    }

    fn build_inner(self, direct: bool) -> Result<BlockingHttpClient, BuildCustomCaTransportError> {
        self.build_inner_using(direct, build_blocking_reqwest_client_with_custom_ca_policy)
    }

    fn build_inner_using(
        self,
        direct: bool,
        build_with_custom_ca: impl FnOnce(
            reqwest::blocking::ClientBuilder,
            CustomCaPolicy,
        ) -> Result<
            reqwest::blocking::Client,
            BuildCustomCaTransportError,
        >,
    ) -> Result<BlockingHttpClient, BuildCustomCaTransportError> {
        let custom_ca_policy = if self.tls_certs_only.is_some() {
            CustomCaPolicy::ExplicitRootSet
        } else {
            CustomCaPolicy::HonorProcessEnvironment
        };
        let mut builder = reqwest::blocking::Client::builder();
        if direct {
            builder = builder.no_proxy();
        }
        if !self.follow_redirects {
            builder = builder.redirect(reqwest::redirect::Policy::none());
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
        build_with_custom_ca(builder, custom_ca_policy).map(|inner| BlockingHttpClient { inner })
    }
}

/// Synchronous HTTP client backed by the repository-owned transport policy.
#[derive(Clone)]
pub struct BlockingHttpClient {
    inner: reqwest::blocking::Client,
}

impl BlockingHttpClient {
    pub fn get(&self, url: impl AsRef<str>) -> BlockingRequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn post(&self, url: impl AsRef<str>) -> BlockingRequestBuilder {
        self.request(Method::POST, url)
    }

    pub fn request(&self, method: Method, url: impl AsRef<str>) -> BlockingRequestBuilder {
        BlockingRequestBuilder {
            inner: self.inner.request(method, url.as_ref()),
        }
    }
}

#[must_use = "requests are not sent until `send` is called"]
pub struct BlockingRequestBuilder {
    inner: reqwest::blocking::RequestBuilder,
}

impl BlockingRequestBuilder {
    pub fn headers(self, headers: HeaderMap) -> Self {
        Self {
            inner: self.inner.headers(headers),
        }
    }

    pub fn body(self, body: impl Into<Vec<u8>>) -> Self {
        Self {
            inner: self.inner.body(body.into()),
        }
    }

    pub fn timeout(self, timeout: Duration) -> Self {
        Self {
            inner: self.inner.timeout(timeout),
        }
    }

    pub fn send(self) -> Result<BlockingHttpResponse, HttpError> {
        self.inner
            .send()
            .map(|inner| BlockingHttpResponse { inner })
    }
}

/// Blocking response that preserves streaming reads without exposing reqwest.
pub struct BlockingHttpResponse {
    inner: reqwest::blocking::Response,
}

impl BlockingHttpResponse {
    pub fn status(&self) -> StatusCode {
        self.inner.status()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    pub fn content_length(&self) -> Option<u64> {
        self.inner.content_length()
    }

    pub fn url(&self) -> &str {
        self.inner.url().as_str()
    }

    pub fn error_for_status(self) -> Result<Self, HttpError> {
        self.inner
            .error_for_status()
            .map(|inner| BlockingHttpResponse { inner })
    }

    pub fn text(self) -> Result<String, HttpError> {
        self.inner.text()
    }

    pub fn bytes(self) -> Result<bytes::Bytes, HttpError> {
        self.inner.bytes()
    }
}

impl Default for BlockingHttpClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Read for BlockingHttpResponse {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}

#[cfg(test)]
#[path = "blocking_client_tests.rs"]
mod tests;
