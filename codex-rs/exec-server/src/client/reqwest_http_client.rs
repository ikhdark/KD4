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
    redirect_policy: HttpRedirectPolicy,
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

    fn cached_client(redirect_policy: HttpRedirectPolicy) -> Option<Arc<SharedHttpClient>> {
        // Cold construction holds this mutex while loading certificates. Never wait
        // for it on an async worker.
        let clients = HTTP_CLIENTS.try_lock().ok()?;
        match redirect_policy {
            HttpRedirectPolicy::Follow => clients.follow_redirects.clone(),
            HttpRedirectPolicy::Stop => clients.stop_redirects.clone(),
        }
    }

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
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms, params.redirect_policy);
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
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms, params.redirect_policy);
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
    pub(crate) fn new(timeout_ms: Option<u64>, redirect_policy: HttpRedirectPolicy) -> Self {
        Self {
            redirect_policy,
            timeout: timeout_ms.map(Duration::from_millis),
        }
    }

    pub(crate) async fn run(
        &self,
        params: HttpRequestParams,
    ) -> Result<(HttpRequestResponse, Option<PendingReqwestHttpBodyStream>), JSONRPCErrorError>
    {
        let deadline = self
            .timeout
            .map(|timeout| {
                tokio::time::Instant::now()
                    .checked_add(timeout)
                    .ok_or_else(|| invalid_params("http/request timeout is too large".into()))
            })
            .transpose()?;
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
        // Client construction can read custom certificates and platform roots. Keep it off the
        // async worker and inside the same deadline as the request; this worker never sends HTTP.
        let redirect_policy = self.redirect_policy;
        let client = if let Some(client) = ReqwestHttpClient::cached_client(redirect_policy) {
            client
        } else {
            let prepare = tokio::task::spawn_blocking(move || {
                ReqwestHttpClient::shared_client(redirect_policy)
            });
            match deadline {
                Some(deadline) => {
                    tokio::time::timeout_at(deadline, prepare)
                        .await
                        .map_err(|_| {
                            internal_error("http/request timed out preparing HTTP client".into())
                        })?
                }
                None => prepare.await,
            }
            .map_err(|error| internal_error(format!("http client preparation failed: {error}")))?
            .map_err(|error| internal_error(error.to_string()))?
        };
        let mut request = client
            .request(method.clone(), params.url.clone())
            .headers(headers);
        if let Some(deadline) = deadline {
            let timeout = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .filter(|timeout| !timeout.is_zero())
                .ok_or_else(|| {
                    internal_error("http/request timed out preparing HTTP client".into())
                })?;
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
        let headers = Self::response_headers(response.headers())?;

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

        let limit = crate::connection::MAX_FILE_PAYLOAD_BYTES;
        let unavailable = || {
            internal_error(format!(
                "http/request was sent and returned status {status}, but its body exceeds the {limit}-byte buffered response limit; use streaming (do not blindly retry the request)"
            ))
        };
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(unavailable());
        }
        let body = codex_http_client::collect_response_body(response.bytes_stream(), limit).await.map_err(|error| {
            if matches!(error, codex_http_client::TransportError::ResponseTooLarge { .. }) {
                return unavailable();
            }
            internal_error(format!(
                "http/request was sent and returned status {status}, but its response body is unavailable: {error}"
            ))
        })?;

        Ok((
            HttpRequestResponse {
                status,
                headers,
                body: body.into(),
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

    fn response_headers(headers: &HeaderMap) -> Result<Vec<HttpHeader>, JSONRPCErrorError> {
        // JSON escaping can expand a byte sixfold. Keep the full header envelope
        // inside the response headroom reserved alongside the base64 body limit.
        let header_bytes = headers.iter().fold(0usize, |total, (name, value)| {
            total
                .saturating_add(name.as_str().len())
                .saturating_add(value.as_bytes().len())
                .saturating_add(64)
        });
        if header_bytes > 128 * 1024 {
            return Err(internal_error(
                "http/request was sent, but response headers exceed the buffered envelope limit"
                    .into(),
            ));
        }
        headers
            .iter()
            .map(|(name, value)| {
                Ok(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_str().map_err(|_| {
                        internal_error(format!("http/request response header `{name}` has an unsupported non-text value"))
                    })?.to_string(),
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

