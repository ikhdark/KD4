//! RMCP Streamable HTTP adapter built on top of the shared `HttpClient`
//! capability.
//!
//! This module runs in the orchestrator process. It turns high-level RMCP
//! operations like `post_message` and `get_stream` into calls on
//! `Arc<dyn HttpClient>`, which may be:
//! - a local HTTP client that issues requests from the orchestrator, or
//! - a remote HTTP client that forwards requests to the remote runtime

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use codex_api::SharedAuthProvider;
use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpResponseBodyStream;
use futures::StreamExt;
use futures::stream;
use futures::stream::BoxStream;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::StatusCode;
use http::header::ACCEPT;
use http::header::AUTHORIZATION;
use http::header::CONTENT_TYPE;
use http::header::WWW_AUTHENTICATE;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::model::ClientNotification;
use rmcp::model::ConstString;
use rmcp::model::JsonRpcMessage;
use rmcp::model::ServerJsonRpcMessage;
use rmcp::transport::streamable_http_client::AuthRequiredError;
use rmcp::transport::streamable_http_client::InsufficientScopeError;
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::transport::streamable_http_client::StreamableHttpPostResponse;
use sse_stream::Sse;
use sse_stream::SseStream;
use url::Url;

mod www_authenticate;

use self::www_authenticate::insufficient_scope_challenge;

const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
const JSON_MIME_TYPE: &str = "application/json";
const HEADER_SESSION_ID: &str = "Mcp-Session-Id";
const NON_JSON_RESPONSE_BODY_PREVIEW_BYTES: usize = 8_192;
const MAX_JSON_RPC_ERROR_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct StreamableHttpClientAdapter {
    http_client: Arc<dyn HttpClient>,
    default_headers: HeaderMap,
    auth_provider: Option<SharedAuthProvider>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum StreamableHttpClientAdapterError {
    #[error("streamable HTTP session expired with 404 Not Found")]
    SessionExpired404,
    #[error(transparent)]
    HttpRequest(#[from] ExecServerError),
    #[error("HTTP {status}: {body_preview}")]
    UnexpectedHttpStatus {
        status: StatusCode,
        body_preview: String,
    },
    #[error("invalid HTTP header: {0}")]
    Header(String),
}

impl StreamableHttpClientAdapter {
    pub(crate) fn new(
        http_client: Arc<dyn HttpClient>,
        default_headers: HeaderMap,
        auth_provider: Option<SharedAuthProvider>,
    ) -> Self {
        Self {
            http_client,
            default_headers,
            auth_provider,
        }
    }
}

impl StreamableHttpClient for StreamableHttpClientAdapter {
    type Error = StreamableHttpClientAdapterError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let (mcp_method, mcp_request_id) = client_jsonrpc_message_fields(&message);
        let has_session_id = session_id.is_some();
        let mut headers = self.default_headers.clone();
        headers.extend(custom_headers);
        self.add_auth_headers(&mut headers);
        insert_header(
            &mut headers,
            ACCEPT,
            [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "),
            StreamableHttpClientAdapterError::Header,
        )?;
        insert_header(
            &mut headers,
            CONTENT_TYPE,
            JSON_MIME_TYPE.to_string(),
            StreamableHttpClientAdapterError::Header,
        )?;
        if let Some(auth_token) = auth_token {
            insert_header(
                &mut headers,
                AUTHORIZATION,
                format!("Bearer {auth_token}"),
                StreamableHttpClientAdapterError::Header,
            )?;
        }
        if let Some(session_id_value) = session_id.as_ref() {
            insert_header(
                &mut headers,
                HeaderName::from_static("mcp-session-id"),
                session_id_value.to_string(),
                StreamableHttpClientAdapterError::Header,
            )?;
        }

        let body = serde_json::to_vec(&message).map_err(StreamableHttpError::Deserialize)?;
        let has_authorization_header = headers.contains_key(AUTHORIZATION);
        let response = self
            .http_client
            .http_request_stream(HttpRequestParams {
                method: "POST".to_string(),
                url: uri.to_string(),
                headers: protocol_headers(&headers)?,
                body: Some(body.into()),
                timeout_ms: None,
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: "buffered-request".to_string(),
                stream_response: true,
            })
            .await;
        let (response, mut body_stream) = match response {
            Ok(response) => response,
            Err(error) => {
                log_post_message_http_error(
                    &uri,
                    mcp_method.as_deref(),
                    mcp_request_id.as_deref(),
                    has_session_id,
                    has_authorization_header,
                );
                return Err(StreamableHttpError::Client(
                    StreamableHttpClientAdapterError::from(error),
                ));
            }
        };

        if response.status == StatusCode::NOT_FOUND.as_u16() && session_id.is_some() {
            return Err(StreamableHttpError::Client(
                StreamableHttpClientAdapterError::SessionExpired404,
            ));
        }
        if response.status == StatusCode::UNAUTHORIZED.as_u16()
            && let Some(header) = response_header(&response.headers, WWW_AUTHENTICATE)
        {
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                header,
            )));
        }
        if response.status == StatusCode::FORBIDDEN.as_u16()
            && let Some(challenge) = insufficient_scope_challenge(&response.headers)
        {
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(
                    challenge.www_authenticate_header,
                    challenge.required_scope,
                ),
            ));
        }
        if matches!(
            StatusCode::from_u16(response.status).ok(),
            Some(StatusCode::ACCEPTED | StatusCode::NO_CONTENT)
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        let content_type = response_header(&response.headers, CONTENT_TYPE);
        let session_id = response_header(&response.headers, HEADER_SESSION_ID);
        if !status_is_success(response.status) {
            let parse_error =
                !retryable_post_response_status(mcp_method.as_deref(), response.status)
                    && content_type
                        .as_deref()
                        .is_some_and(|content_type| has_media_type(content_type, JSON_MIME_TYPE));
            let limit = if parse_error {
                MAX_JSON_RPC_ERROR_BODY_BYTES
            } else {
                NON_JSON_RESPONSE_BODY_PREVIEW_BYTES
            };
            let (body, truncated) = collect_body_prefix(&mut body_stream, limit).await?;
            if parse_error
                && !truncated
                && let Some(message) = parse_json_rpc_error(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(message, session_id));
            }
            let mut preview = body_preview(String::from_utf8_lossy(
                &body[..body.len().min(NON_JSON_RESPONSE_BODY_PREVIEW_BYTES)],
            ));
            if truncated {
                preview.push_str(&format!(
                    "... (HTTP error body exceeds {limit}-byte collection limit)"
                ));
            }
            return Err(unexpected_http_status_error(response.status, preview));
        }
        match content_type.as_deref() {
            Some(content_type) if has_media_type(content_type, EVENT_STREAM_MIME_TYPE) => {
                let event_stream = sse_stream_from_body(body_stream);
                Ok(StreamableHttpPostResponse::Sse(event_stream, session_id))
            }
            Some(content_type) if has_media_type(content_type, JSON_MIME_TYPE) => {
                let body = collect_body(&mut body_stream).await?;
                let message: ServerJsonRpcMessage =
                    serde_json::from_slice(&body).map_err(StreamableHttpError::Deserialize)?;
                Ok(StreamableHttpPostResponse::Json(message, session_id))
            }
            _ => {
                let (body, truncated) =
                    collect_body_prefix(&mut body_stream, NON_JSON_RESPONSE_BODY_PREVIEW_BYTES)
                        .await?;
                let content_type = content_type.unwrap_or_else(|| "missing-content-type".into());
                let suffix = if truncated {
                    "... (body preview truncated)"
                } else {
                    ""
                };
                Err(StreamableHttpError::UnexpectedContentType(Some(format!(
                    "{content_type}; body: {}{suffix}",
                    body_preview(String::from_utf8_lossy(&body))
                ))))
            }
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<(), StreamableHttpError<Self::Error>> {
        let mut headers = self.default_headers.clone();
        headers.extend(custom_headers);
        self.add_auth_headers(&mut headers);
        if let Some(auth_token) = auth_token {
            insert_header(
                &mut headers,
                AUTHORIZATION,
                format!("Bearer {auth_token}"),
                StreamableHttpClientAdapterError::Header,
            )?;
        }
        insert_header(
            &mut headers,
            HeaderName::from_static("mcp-session-id"),
            session.to_string(),
            StreamableHttpClientAdapterError::Header,
        )?;

        let response = self
            .http_client
            .http_request(HttpRequestParams {
                method: "DELETE".to_string(),
                url: uri.to_string(),
                headers: protocol_headers(&headers)?,
                body: None,
                timeout_ms: None,
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: "buffered-request".to_string(),
                stream_response: false,
            })
            .await
            .map_err(StreamableHttpClientAdapterError::from)
            .map_err(StreamableHttpError::Client)?;

        if response.status == StatusCode::METHOD_NOT_ALLOWED.as_u16() {
            return Ok(());
        }
        if !status_is_success(response.status) {
            return Err(unexpected_http_status_error(
                response.status,
                "DELETE request failed".to_string(),
            ));
        }
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<
        BoxStream<'static, std::result::Result<Sse, sse_stream::Error>>,
        StreamableHttpError<Self::Error>,
    > {
        let mut headers = self.default_headers.clone();
        headers.extend(custom_headers);
        self.add_auth_headers(&mut headers);
        insert_header(
            &mut headers,
            ACCEPT,
            EVENT_STREAM_MIME_TYPE.to_string(),
            StreamableHttpClientAdapterError::Header,
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("mcp-session-id"),
            session_id.to_string(),
            StreamableHttpClientAdapterError::Header,
        )?;
        if let Some(last_event_id) = last_event_id {
            insert_header(
                &mut headers,
                HeaderName::from_static("last-event-id"),
                last_event_id,
                StreamableHttpClientAdapterError::Header,
            )?;
        }
        if let Some(auth_token) = auth_token {
            insert_header(
                &mut headers,
                AUTHORIZATION,
                format!("Bearer {auth_token}"),
                StreamableHttpClientAdapterError::Header,
            )?;
        }

        let (response, body_stream) = self
            .http_client
            .http_request_stream(HttpRequestParams {
                method: "GET".to_string(),
                url: uri.to_string(),
                headers: protocol_headers(&headers)?,
                body: None,
                timeout_ms: None,
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: "buffered-request".to_string(),
                stream_response: true,
            })
            .await
            .map_err(StreamableHttpClientAdapterError::from)
            .map_err(StreamableHttpError::Client)?;

        if response.status == StatusCode::METHOD_NOT_ALLOWED.as_u16() {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if response.status == StatusCode::NOT_FOUND.as_u16() {
            return Err(StreamableHttpError::Client(
                StreamableHttpClientAdapterError::SessionExpired404,
            ));
        }
        if !status_is_success(response.status) {
            return Err(unexpected_http_status_error(
                response.status,
                "GET request failed".to_string(),
            ));
        }

        match response_header(&response.headers, CONTENT_TYPE).as_deref() {
            Some(content_type) if has_media_type(content_type, EVENT_STREAM_MIME_TYPE) => {}
            Some(content_type) => {
                return Err(StreamableHttpError::UnexpectedContentType(Some(
                    content_type.to_string(),
                )));
            }
            None => {
                return Err(StreamableHttpError::UnexpectedContentType(None));
            }
        }

        Ok(sse_stream_from_body(body_stream))
    }
}

impl StreamableHttpClientAdapter {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        if let Some(auth_provider) = &self.auth_provider {
            headers.extend(auth_provider.to_auth_headers());
        }
    }
}

fn body_preview(body: impl Into<String>) -> String {
    let mut body_preview = body.into();
    let body_len = body_preview.len();
    if body_len > NON_JSON_RESPONSE_BODY_PREVIEW_BYTES {
        let mut boundary = NON_JSON_RESPONSE_BODY_PREVIEW_BYTES;
        while !body_preview.is_char_boundary(boundary) {
            boundary = boundary.saturating_sub(1);
        }
        body_preview.truncate(boundary);
        body_preview.push_str(&format!(
            "... (truncated {} bytes)",
            body_len.saturating_sub(boundary)
        ));
    }
    body_preview
}

fn client_jsonrpc_message_fields(
    message: &ClientJsonRpcMessage,
) -> (Option<String>, Option<String>) {
    match message {
        JsonRpcMessage::Request(request) => (
            Some(request.request.method().to_string()),
            Some(request.id.to_string()),
        ),
        JsonRpcMessage::Response(response) => (None, Some(response.id.to_string())),
        JsonRpcMessage::Notification(notification) => {
            let method = match &notification.notification {
                ClientNotification::CancelledNotification(notification) => {
                    notification.method.as_str()
                }
                ClientNotification::ProgressNotification(notification) => {
                    notification.method.as_str()
                }
                ClientNotification::InitializedNotification(notification) => {
                    notification.method.as_str()
                }
                ClientNotification::RootsListChangedNotification(notification) => {
                    notification.method.as_str()
                }
                ClientNotification::CustomNotification(notification) => {
                    notification.method.as_str()
                }
            };
            (Some(method.to_string()), None)
        }
        JsonRpcMessage::Error(error) => (None, error.id.as_ref().map(ToString::to_string)),
    }
}

fn log_post_message_http_error(
    uri: &str,
    mcp_method: Option<&str>,
    mcp_request_id: Option<&str>,
    has_session_id: bool,
    has_authorization_header: bool,
) {
    let parsed_url = Url::parse(uri).ok();
    tracing::warn!(
        endpoint_scheme = parsed_url.as_ref().map(Url::scheme).unwrap_or("<invalid>"),
        endpoint_host = parsed_url
            .as_ref()
            .and_then(Url::host_str)
            .unwrap_or("<invalid>"),
        endpoint_path = parsed_url.as_ref().map(Url::path).unwrap_or("<invalid>"),
        endpoint_has_query = parsed_url.as_ref().is_some_and(|url| url.query().is_some()),
        mcp_method = mcp_method.unwrap_or("<none>"),
        mcp_request_id = mcp_request_id.unwrap_or("<none>"),
        has_session_id = has_session_id,
        has_authorization_header = has_authorization_header,
        "streamable HTTP post_message failed"
    );
}

fn insert_header<Error>(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: String,
    map_error: impl FnOnce(String) -> Error,
) -> std::result::Result<(), StreamableHttpError<Error>>
where
    Error: std::error::Error + Send + Sync + 'static,
{
    let value = HeaderValue::from_str(&value)
        .map_err(|error| StreamableHttpError::Client(map_error(error.to_string())))?;
    headers.insert(name, value);
    Ok(())
}

fn has_media_type(content_type: &str, expected: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case(expected))
}

fn protocol_headers(
    headers: &HeaderMap,
) -> Result<Vec<HttpHeader>, StreamableHttpError<StreamableHttpClientAdapterError>> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value.to_str().map_err(|_| {
                StreamableHttpError::Client(StreamableHttpClientAdapterError::Header(format!(
                    "header `{name}` cannot be represented as an ASCII HTTP header"
                )))
            })?;
            Ok(HttpHeader {
                name: name.as_str().to_string(),
                value: value.to_string(),
            })
        })
        .collect()
}

fn response_header(headers: &[HttpHeader], name: impl AsRef<str>) -> Option<String> {
    let name = name.as_ref();
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.clone())
}

fn status_is_success(status: u16) -> bool {
    StatusCode::from_u16(status).is_ok_and(|status| status.is_success())
}

fn retryable_post_response_status(mcp_method: Option<&str>, status: u16) -> bool {
    let Ok(status) = StatusCode::from_u16(status) else {
        return false;
    };
    is_retryable_http_status(status)
        && matches!(
            mcp_method,
            Some(
                "initialize"
                    | "notifications/initialized"
                    | "tools/list"
                    | "resources/list"
                    | "resources/templates/list"
                    | "resources/read"
            )
        )
}

pub(crate) fn is_retryable_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn unexpected_http_status_error(
    status: u16,
    body_preview: String,
) -> StreamableHttpError<StreamableHttpClientAdapterError> {
    match StatusCode::from_u16(status) {
        Ok(status) => {
            StreamableHttpError::Client(StreamableHttpClientAdapterError::UnexpectedHttpStatus {
                status,
                body_preview,
            })
        }
        Err(_) => StreamableHttpError::UnexpectedServerResponse(
            format!("invalid HTTP status {status}: {body_preview}").into(),
        ),
    }
}

fn parse_json_rpc_error(body: &[u8]) -> Option<ServerJsonRpcMessage> {
    match serde_json::from_slice::<ServerJsonRpcMessage>(body) {
        Ok(message @ JsonRpcMessage::Error(_)) => Some(message),
        _ => None,
    }
}

async fn collect_body_prefix(
    body_stream: &mut HttpResponseBodyStream,
    limit: usize,
) -> std::result::Result<(Vec<u8>, bool), StreamableHttpError<StreamableHttpClientAdapterError>> {
    let mut body = Vec::new();
    while let Some(chunk) = body_stream
        .recv()
        .await
        .map_err(StreamableHttpClientAdapterError::from)
        .map_err(StreamableHttpError::Client)?
    {
        let remaining = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

async fn collect_body(
    body_stream: &mut HttpResponseBodyStream,
) -> std::result::Result<Vec<u8>, StreamableHttpError<StreamableHttpClientAdapterError>> {
    let mut body = Vec::new();
    while let Some(chunk) = body_stream
        .recv()
        .await
        .map_err(StreamableHttpClientAdapterError::from)
        .map_err(StreamableHttpError::Client)?
    {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn sse_stream_from_body(
    body_stream: HttpResponseBodyStream,
) -> BoxStream<'static, std::result::Result<Sse, sse_stream::Error>> {
    SseStream::from_bytes_stream(stream::unfold(body_stream, |mut body_stream| async move {
        match body_stream.recv().await {
            Ok(Some(bytes)) => Some((Ok(Bytes::from(bytes)), body_stream)),
            Ok(None) => None,
            Err(error) => Some((Err(io::Error::other(error)), body_stream)),
        }
    }))
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::any;

    #[tokio::test]
    async fn transport_bounds_errors_and_preserves_large_successes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/{case}", any(|axum::extract::Path(case): axum::extract::Path<String>| async move {
            let (status, content_type, body) = match case.as_str() {
                "error" => (StatusCode::SERVICE_UNAVAILABLE, "text/plain", Body::from_stream(
                    stream::once(async { Ok::<_, io::Error>(vec![b'x'; NON_JSON_RESPONSE_BODY_PREVIEW_BYTES + 1]) })
                        .chain(stream::pending()))),
                "rpc-error" => (StatusCode::BAD_REQUEST, JSON_MIME_TYPE, Body::from_stream(
                    stream::once(async { Ok::<_, io::Error>(vec![b'x'; MAX_JSON_RPC_ERROR_BODY_BYTES + 1]) })
                        .chain(stream::pending()))),
                "invalid-type" => (StatusCode::OK, "application/json-invalid", Body::from("{}")),
                "json-get" => (StatusCode::OK, JSON_MIME_TYPE, Body::from("{}")),
                "rpc" => (StatusCode::BAD_REQUEST, "Application/JSON; charset=utf-8", Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid arguments"}}"#)),
                _ => (StatusCode::OK, "Application/JSON; charset=utf-8", Body::from(serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "result": {"payload": "x".repeat(MAX_JSON_RPC_ERROR_BODY_BYTES + 1)}
                }).to_string())),
            };
            (status, [(CONTENT_TYPE, content_type)], body)
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let adapter = StreamableHttpClientAdapter::new(
            Arc::new(codex_exec_server::ReqwestHttpClient),
            HeaderMap::new(),
            None,
        );
        for case in ["error", "rpc-error", "invalid-type", "rpc", "success"] {
            let message = serde_json::from_value(
                serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"}),
            )
            .unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                adapter.post_message(
                    format!("http://{address}/{case}").into(),
                    message,
                    None,
                    None,
                    HashMap::new(),
                ),
            )
            .await
            .expect("response processing must finish without EOF for oversized errors");
            match (case, result) {
                (
                    "error" | "rpc-error",
                    Err(StreamableHttpError::Client(
                        StreamableHttpClientAdapterError::UnexpectedHttpStatus {
                            body_preview, ..
                        },
                    )),
                ) => {
                    assert!(body_preview.len() < NON_JSON_RESPONSE_BODY_PREVIEW_BYTES + 200);
                    assert!(body_preview.contains("collection limit"));
                }
                ("invalid-type", Err(StreamableHttpError::UnexpectedContentType(_))) => {}
                ("rpc", Ok(StreamableHttpPostResponse::Json(JsonRpcMessage::Error(error), _))) => {
                    assert_eq!(error.error.message, "invalid arguments");
                }
                ("success", Ok(StreamableHttpPostResponse::Json(message, _))) => {
                    assert_eq!(
                        serde_json::to_value(message).unwrap()["result"]["payload"]
                            .as_str()
                            .unwrap()
                            .len(),
                        MAX_JSON_RPC_ERROR_BODY_BYTES + 1
                    );
                }
                _ => panic!("unexpected response for {case}"),
            }
        }
        let result = adapter
            .get_stream(
                format!("http://{address}/json-get").into(),
                "session".into(),
                None,
                None,
                HashMap::new(),
            )
            .await;
        server.abort();
        assert!(matches!(
            result,
            Err(StreamableHttpError::UnexpectedContentType(_))
        ));
    }
}
