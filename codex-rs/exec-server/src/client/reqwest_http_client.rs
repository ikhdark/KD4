//! Shared-transport-backed `HttpClient` implementation.
//!
//! This code runs wherever the real network request should originate:
//! - in a local environment, that means the orchestrator process
//! - in a remote environment, that means the remote runtime after the
//!   orchestrator has forwarded `http/request` over JSON-RPC

use std::error::Error as StdError;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_http_client::HttpClient as SharedHttpClient;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpError;
use codex_http_client::HttpResponse;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use http::Uri;
use tracing::Instrument;

use super::HttpResponseBodyStream;
use super::response_body_stream::send_body_delta;
use crate::HttpClient;
use crate::client::ExecServerError;
use crate::protocol::HttpHeader;
use crate::protocol::HttpRedirectPolicy;
use crate::protocol::HttpRequestBodyDeltaNotification;
use crate::protocol::HttpRequestParams;
use crate::protocol::HttpRequestResponse;
use crate::rpc::RpcNotificationSender;
use crate::rpc::internal_error;
use crate::rpc::invalid_params;

/// `HttpClient` implementation that performs the actual HTTP request with
/// the repository-owned HTTP transport.
#[derive(Clone, Default)]
pub struct ReqwestHttpClient;

/// Streaming response state held between the initial HTTP response and
/// downstream body-delta forwarding.
pub(crate) struct PendingReqwestHttpBodyStream {
    pub(crate) request_id: String,
    pub(crate) response: HttpResponse,
}

/// Validates `http/request` parameters and runs the actual HTTP call used
/// by the exec-server route and the local [`HttpClient`] backend.
pub(crate) struct ReqwestHttpRequestRunner {
    client: Arc<SharedHttpClient>,
    timeout: Option<Duration>,
}

#[derive(Default)]
struct ReqwestHttpClients {
    follow_redirects: Option<Arc<SharedHttpClient>>,
    stop_redirects: Option<Arc<SharedHttpClient>>,
}

static HTTP_CLIENTS: LazyLock<Mutex<ReqwestHttpClients>> =
    LazyLock::new(|| Mutex::new(ReqwestHttpClients::default()));

impl ReqwestHttpClient {
    fn build_client(
        redirect_policy: HttpRedirectPolicy,
    ) -> Result<SharedHttpClient, ExecServerError> {
        let builder = HttpClientBuilder::new().with_chatgpt_cloudflare_cookie_store();
        let builder = match redirect_policy {
            HttpRedirectPolicy::Follow => builder,
            HttpRedirectPolicy::Stop => builder.without_redirects(),
        };
        builder
            .build_with_transport_default_proxy()
            .map_err(|error| ExecServerError::HttpRequest(error.to_string()))
    }

    fn shared_client(
        redirect_policy: HttpRedirectPolicy,
    ) -> Result<Arc<SharedHttpClient>, ExecServerError> {
        let mut clients = HTTP_CLIENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = match redirect_policy {
            HttpRedirectPolicy::Follow => &mut clients.follow_redirects,
            HttpRedirectPolicy::Stop => &mut clients.stop_redirects,
        };
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }

        let client = Arc::new(Self::build_client(redirect_policy)?);
        *slot = Some(client.clone());
        Ok(client)
    }
}

impl HttpClient for ReqwestHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        async move {
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms, params.redirect_policy)
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let (response, _) = runner
                .run(HttpRequestParams {
                    stream_response: false,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            Ok(response)
        }
        .boxed()
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        async move {
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms, params.redirect_policy)
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let (response, pending_stream) = runner
                .run(HttpRequestParams {
                    stream_response: true,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let pending_stream = pending_stream.ok_or_else(|| {
                ExecServerError::Protocol(
                    "http request stream did not return a response body stream".to_string(),
                )
            })?;
            Ok((
                response,
                HttpResponseBodyStream::local(pending_stream.response),
            ))
        }
        .boxed()
    }
}

impl ReqwestHttpRequestRunner {
    pub(crate) fn new(
        timeout_ms: Option<u64>,
        redirect_policy: HttpRedirectPolicy,
    ) -> Result<Self, JSONRPCErrorError> {
        let client = ReqwestHttpClient::shared_client(redirect_policy)
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(Self {
            client,
            timeout: timeout_ms.map(Duration::from_millis),
        })
    }

    pub(crate) async fn run(
        &self,
        params: HttpRequestParams,
    ) -> Result<(HttpRequestResponse, Option<PendingReqwestHttpBodyStream>), JSONRPCErrorError>
    {
        let method = Method::from_bytes(params.method.as_bytes())
            .map_err(|error| invalid_params(format!("http/request method is invalid: {error}")))?;
        let url = params
            .url
            .parse::<Uri>()
            .map_err(|error| invalid_params(format!("http/request url is invalid: {error}")))?;
        match url.scheme_str() {
            Some("http") | Some("https") => {}
            scheme => {
                return Err(invalid_params(format!(
                    "http/request only supports http and https URLs, got {}",
                    scheme.unwrap_or("<missing>")
                )));
            }
        }

        let request_span = tracing::info_span!(
            "codex.exec_server.http_request",
            otel.kind = "client",
            http.request.method = method.as_str(),
            server.address = url.host().unwrap_or_default(),
            server.port = u64::from(url.port_u16().unwrap_or_else(|| if url.scheme_str() == Some("https") { 443 } else { 80 })),
            http.response.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let mut headers = Self::build_headers(params.headers)?;
        codex_otel::inject_span_w3c_trace_headers(&request_span, &mut headers);
        let mut request = self
            .client
            .request(method.clone(), params.url.clone())
            .headers(headers);
        if let Some(timeout) = self.timeout {
            request = request.timeout(timeout);
        }
        if let Some(body) = params.body {
            request = request.body(body.into_inner());
        }

        let response = match request.send().instrument(request_span.clone()).await {
            Ok(response) => response,
            Err(error) => {
                request_span.record("error.type", "request");
                let error_message = error.to_string();
                log_send_error(&method, error);
                return Err(internal_error(format!(
                    "http/request failed: {error_message}"
                )));
            }
        };
        let status = response.status().as_u16();
        request_span.record("http.response.status_code", u64::from(status));
        let headers = Self::response_headers(response.headers());

        if params.stream_response {
            return Ok((
                HttpRequestResponse {
                    status,
                    headers,
                    body: Vec::new().into(),
                },
                Some(PendingReqwestHttpBodyStream {
                    request_id: params.request_id,
                    response,
                }),
            ));
        }

        let body = response.bytes().await.map_err(|error| {
            internal_error(format!(
                "failed to read http/request response body: {error}"
            ))
        })?;

        Ok((
            HttpRequestResponse {
                status,
                headers,
                body: body.to_vec().into(),
            },
            None,
        ))
    }

    pub(crate) async fn stream_body(
        pending_stream: PendingReqwestHttpBodyStream,
        notifications: RpcNotificationSender,
    ) {
        let PendingReqwestHttpBodyStream {
            request_id,
            response,
        } = pending_stream;
        let mut seq = 1;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    if !send_body_delta(
                        &notifications,
                        HttpRequestBodyDeltaNotification {
                            request_id: request_id.clone(),
                            seq,
                            delta: bytes.to_vec().into(),
                            done: false,
                            error: None,
                        },
                    )
                    .await
                    {
                        return;
                    }
                    seq += 1;
                }
                Err(error) => {
                    let _ = send_body_delta(
                        &notifications,
                        HttpRequestBodyDeltaNotification {
                            request_id,
                            seq,
                            delta: Vec::new().into(),
                            done: true,
                            error: Some(error.to_string()),
                        },
                    )
                    .await;
                    return;
                }
            }
        }

        let _ = send_body_delta(
            &notifications,
            HttpRequestBodyDeltaNotification {
                request_id,
                seq,
                delta: Vec::new().into(),
                done: true,
                error: None,
            },
        )
        .await;
    }

    fn build_headers(headers: Vec<HttpHeader>) -> Result<HeaderMap, JSONRPCErrorError> {
        let mut header_map = HeaderMap::new();
        for header in headers {
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|error| {
                invalid_params(format!("http/request header name is invalid: {error}"))
            })?;
            let value = HeaderValue::from_str(&header.value).map_err(|error| {
                invalid_params(format!(
                    "http/request header value is invalid for {}: {error}",
                    header.name
                ))
            })?;
            header_map.append(name, value);
        }
        Ok(header_map)
    }

    fn response_headers(headers: &HeaderMap) -> Vec<HttpHeader> {
        headers
            .iter()
            .filter_map(|(name, value)| {
                Some(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_str().ok()?.to_string(),
                })
            })
            .collect()
    }
}

fn log_send_error(method: &Method, error: HttpError) {
    let error = error.without_url();
    let source_chain = error_source_chain(&error);
    tracing::warn!(
        http_method = method.as_str(),
        error_is_timeout = error.is_timeout(),
        error_is_connect = error.is_connect(),
        error = %error,
        error_sources = ?source_chain,
        "http/request send failed"
    );
}

fn error_source_chain(error: &HttpError) -> Option<String> {
    let mut sources = Vec::new();
    let mut source = error.source();
    while let Some(error) = source {
        sources.push(error.to_string());
        source = error.source();
    }
    (!sources.is_empty()).then(|| sources.join(": "))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::*;

    #[test]
    fn request_runners_reuse_client_per_redirect_policy() {
        let first = ReqwestHttpClient::shared_client(HttpRedirectPolicy::Follow)
            .expect("build first HTTP client");
        let second = ReqwestHttpClient::shared_client(HttpRedirectPolicy::Follow)
            .expect("reuse HTTP client");
        let stop = ReqwestHttpClient::shared_client(HttpRedirectPolicy::Stop)
            .expect("build no-redirect HTTP client");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &stop));
    }

    #[tokio::test]
    async fn request_runner_uses_shared_client_for_buffered_http_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/shared-client"))
            .and(header("x-codex-test", "exec-server"))
            .respond_with(ResponseTemplate::new(201).set_body_bytes(b"created".to_vec()))
            .mount(&server)
            .await;
        let runner = ReqwestHttpRequestRunner::new(Some(2_000), HttpRedirectPolicy::Follow)
            .expect("build request runner");

        let (response, pending_stream) = runner
            .run(HttpRequestParams {
                method: "POST".to_string(),
                url: format!("{}/shared-client", server.uri()),
                headers: vec![HttpHeader {
                    name: "x-codex-test".to_string(),
                    value: "exec-server".to_string(),
                }],
                body: Some(b"payload".to_vec().into()),
                timeout_ms: Some(2_000),
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: "request-1".to_string(),
                stream_response: false,
            })
            .await
            .expect("run request");

        assert_eq!(response.status, 201);
        assert_eq!(response.body.into_inner(), b"created".to_vec());
        assert!(pending_stream.is_none());
    }
}
