use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use http::HeaderMap;
use oauth2::HttpRequest;
use oauth2::HttpResponse;
use rmcp::transport::auth::OAuthHttpClient;
use rmcp::transport::auth::OAuthHttpClientError;
use rmcp::transport::auth::OAuthHttpClientFuture;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use rmcp::transport::auth::OAuthHttpRequest;

pub(crate) const MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES: usize = 1024 * 1024;
static NEXT_OAUTH_REQUEST_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(crate) struct OAuthHttpClientAdapter {
    http_client: Arc<dyn HttpClient>,
    default_headers: HeaderMap,
    resource_origin: url::Origin,
    buffered_responses: bool,
    timeout_cap: Option<Duration>,
}

impl OAuthHttpClientAdapter {
    pub(crate) fn new(
        http_client: Arc<dyn HttpClient>,
        resource_url: &str,
        default_headers: HeaderMap,
    ) -> Result<Self, OAuthHttpClientError> {
        let resource = url::Url::parse(resource_url)
            .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
        if !matches!(resource.scheme(), "http" | "https") || resource.host_str().is_none() {
            return Err(OAuthHttpClientError::from(
                "MCP resource URL must be HTTP(S) with a host",
            ));
        }
        Ok(Self {
            http_client,
            default_headers,
            resource_origin: resource.origin(),
            buffered_responses: false,
            timeout_cap: None,
        })
    }

    pub(crate) fn with_buffered_responses(mut self) -> Self {
        self.buffered_responses = true;
        self
    }

    /// Bounds every request, including one for which rmcp suggests no timeout.
    pub(crate) fn with_timeout_cap(mut self, cap: Duration) -> Self {
        self.timeout_cap = Some(cap);
        self
    }

    async fn execute_request(
        &self,
        request: HttpRequest,
        redirect_policy: OAuthHttpRedirectPolicy,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, OAuthHttpClientError> {
        let follow = match redirect_policy {
            OAuthHttpRedirectPolicy::Follow => true,
            OAuthHttpRedirectPolicy::Stop => false,
            _ => {
                return Err(OAuthHttpClientError::from(
                    "unsupported OAuth HTTP redirect policy",
                ));
            }
        };
        let timeout = match self.timeout_cap {
            Some(cap) => Some(timeout.map_or(cap, |value| value.min(cap))),
            None => timeout,
        };
        let deadline = timeout
            .map(|value| {
                tokio::time::Instant::now()
                    .checked_add(value)
                    .ok_or_else(|| OAuthHttpClientError::from("OAuth HTTP timeout is too large"))
            })
            .transpose()?;
        let (mut parts, mut body) = request.into_parts();
        let mut url = url::Url::parse(&parts.uri.to_string())
            .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
        for hop in 0..=10 {
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(OAuthHttpClientError::from(
                    "OAuth HTTP requires an HTTP(S) URL without userinfo",
                ));
            }
            let remaining = deadline
                .map(|deadline| {
                    deadline
                        .checked_duration_since(tokio::time::Instant::now())
                        .filter(|value| !value.is_zero())
                        .ok_or_else(|| OAuthHttpClientError::from("OAuth HTTP request timed out"))
                })
                .transpose()?;
            let mut request = oauth2::http::Request::builder()
                .method(parts.method.clone())
                .uri(url.as_str())
                .body(body.clone())
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            *request.headers_mut() = parts.headers.clone();
            let response = match deadline {
                Some(deadline) => {
                    tokio::time::timeout_at(deadline, self.execute_once(request, remaining))
                        .await
                        .map_err(|_| OAuthHttpClientError::from("OAuth HTTP request timed out"))??
                }
                None => self.execute_once(request, remaining).await?,
            };
            let status = response.status().as_u16();
            if !follow || !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response.headers().get(http::header::LOCATION) else {
                return Ok(response);
            };
            if hop == 10 {
                return Err(OAuthHttpClientError::from("too many OAuth HTTP redirects"));
            }
            let location = location
                .to_str()
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            let next = url
                .join(location)
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            if next.origin() != url.origin() {
                for name in [
                    "authorization",
                    "cookie",
                    "cookie2",
                    "proxy-authorization",
                    "www-authenticate",
                    "host",
                    "referer",
                ] {
                    parts.headers.remove(name);
                }
                let sensitive = parts
                    .headers
                    .iter()
                    .filter(|(_, value)| value.is_sensitive())
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                for name in sensitive {
                    parts.headers.remove(name);
                }
            }
            if ((status == 301 || status == 302) && parts.method == http::Method::POST)
                || (status == 303 && parts.method != http::Method::HEAD)
            {
                parts.method = http::Method::GET;
                body.clear();
                for name in [
                    "content-type",
                    "content-length",
                    "content-encoding",
                    "transfer-encoding",
                ] {
                    parts.headers.remove(name);
                }
            }
            url = next;
        }
        unreachable!("redirect loop returns at its bound")
    }

    async fn execute_once(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, OAuthHttpClientError> {
        let (parts, body) = request.into_parts();
        let target = url::Url::parse(&parts.uri.to_string())
            .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
        let mut headers = if target.origin() == self.resource_origin {
            self.default_headers.clone()
        } else {
            HeaderMap::new()
        };
        for name in parts.headers.keys() {
            headers.remove(name);
        }
        headers.extend(parts.headers);
        let headers = headers
            .iter()
            .map(|(name, value)| {
                Ok(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value
                        .to_str()
                        .map_err(|error| OAuthHttpClientError::from(error.to_string()))?
                        .to_string(),
                })
            })
            .collect::<Result<Vec<_>, OAuthHttpClientError>>()?;
        let timeout = match self.timeout_cap {
            Some(cap) => Some(timeout.map_or(cap, |timeout| timeout.min(cap))),
            None => timeout,
        };
        let timeout_ms = timeout.map(|timeout| {
            u64::try_from(timeout.as_millis())
                .unwrap_or(u64::MAX)
                .max(1)
        });
        let request_id = NEXT_OAUTH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let params = HttpRequestParams {
            method: parts.method.to_string(),
            url: parts.uri.to_string(),
            headers,
            body: (!body.is_empty()).then_some(body.into()),
            timeout_ms,
            redirect_policy: HttpRedirectPolicy::Stop,
            request_id: format!("oauth-request-{request_id}"),
            stream_response: !self.buffered_responses,
        };
        let (response, body) = if self.buffered_responses {
            let mut response = self
                .http_client
                .http_request(params)
                .await
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            let body = std::mem::replace(&mut response.body, Vec::new().into()).into_inner();
            (response, body)
        } else {
            let (response, mut body_stream) = self
                .http_client
                .http_request_stream(params)
                .await
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
            let mut body = Vec::new();
            while let Some(chunk) = body_stream
                .recv()
                .await
                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?
            {
                if chunk.len() > MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES - body.len() {
                    return Err(OAuthHttpClientError::from(format!(
                        "OAuth HTTP response body exceeds {MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            (response, body)
        };
        if body.len() > MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES {
            return Err(OAuthHttpClientError::from(format!(
                "OAuth HTTP response body exceeds {MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES} bytes"
            )));
        }
        let mut builder = oauth2::http::Response::builder().status(response.status);
        for header in response.headers {
            builder = builder.header(header.name, header.value);
        }
        builder
            .body(body)
            .map_err(|error| OAuthHttpClientError::from(error.to_string()))
    }
}

impl OAuthHttpClient for OAuthHttpClientAdapter {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(self.execute_request(request.request, request.redirect_policy, request.timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_exec_server::{ExecServerError, HttpRequestResponse, HttpResponseBodyStream};

    #[derive(Default)]
    struct RecordingClient(std::sync::Mutex<Vec<HttpRequestParams>>);

    impl HttpClient for RecordingClient {
        fn http_request(
            &self,
            params: HttpRequestParams,
        ) -> futures::future::BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
            let redirect = params.url.ends_with("/redirect");
            self.0.lock().unwrap().push(params);
            Box::pin(async move {
                Ok(HttpRequestResponse {
                    status: if redirect { 302 } else { 200 },
                    headers: if redirect {
                        vec![HttpHeader {
                            name: "location".into(),
                            value: "https://auth.example/token".into(),
                        }]
                    } else {
                        Vec::new()
                    },
                    body: Vec::new().into(),
                })
            })
        }

        fn http_request_stream(
            &self,
            _: HttpRequestParams,
        ) -> futures::future::BoxFuture<
            '_,
            Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>,
        > {
            Box::pin(async { Err(ExecServerError::Protocol("buffered fixture only".into())) })
        }
    }

    #[tokio::test]
    async fn resource_headers_are_origin_scoped_and_do_not_follow_redirects() {
        let client = Arc::new(RecordingClient::default());
        let mut defaults = HeaderMap::new();
        defaults.insert(
            "x-api-key",
            http::HeaderValue::from_static("resource-secret"),
        );
        defaults.insert("cookie", http::HeaderValue::from_static("resource-cookie"));
        defaults.insert(
            "authorization",
            http::HeaderValue::from_static("resource-auth"),
        );
        let adapter =
            OAuthHttpClientAdapter::new(client.clone(), "https://mcp.example/mcp", defaults)
                .unwrap()
                .with_buffered_responses();
        for (uri, same_origin) in [
            ("https://mcp.example/discovery", true),
            ("https://mcp.example:443/discovery", true),
            ("https://auth.example/token", false),
            ("http://mcp.example/token", false),
            ("https://mcp.example:444/token", false),
        ] {
            for override_auth in [false, true] {
                let mut request = oauth2::http::Request::builder().uri(uri);
                if override_auth {
                    request = request.header("authorization", "Basic oauth-owned");
                }
                adapter
                    .execute_request(
                        request.body(Vec::new()).unwrap(),
                        OAuthHttpRedirectPolicy::Stop,
                        None,
                    )
                    .await
                    .unwrap();
                let requests = client.0.lock().unwrap();
                let last = requests.last().unwrap();
                assert_eq!(last.redirect_policy, HttpRedirectPolicy::Stop);
                let header = |name| {
                    last.headers
                        .iter()
                        .find(|header| header.name == name)
                        .map(|header| header.value.as_str())
                };
                assert_eq!(
                    header("x-api-key"),
                    same_origin.then_some("resource-secret")
                );
                assert_eq!(header("cookie"), same_origin.then_some("resource-cookie"));
                assert_eq!(
                    header("authorization"),
                    if override_auth {
                        Some("Basic oauth-owned")
                    } else {
                        same_origin.then_some("resource-auth")
                    }
                );
            }
        }
        adapter
            .execute_request(
                oauth2::http::Request::builder()
                    .uri("https://mcp.example/redirect")
                    .header("authorization", "Basic oauth-owned")
                    .body(Vec::new())
                    .unwrap(),
                OAuthHttpRedirectPolicy::Follow,
                Some(Duration::from_secs(1)),
            )
            .await
            .unwrap();
        let requests = client.0.lock().unwrap();
        let final_request = requests.last().unwrap();
        assert_eq!(final_request.url, "https://auth.example/token");
        assert!(!final_request.headers.iter().any(|header| matches!(
            header.name.as_str(),
            "x-api-key" | "cookie" | "authorization"
        )));
    }
}
