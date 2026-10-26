use std::pin::Pin;
use std::time::Duration;

use crate::AuthError;
use crate::AuthProvider;
use bytes::Bytes;
use codex_http_client::BuildRouteAwareHttpClientError;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_http_client::RouteAwareClientPoolError;
use codex_http_client::RouteAwareRequestBuilder;
use codex_http_client::RouteAwareRequestError;
use futures::Stream;
use http::Method;
use http::StatusCode;
use http::header::CONTENT_LENGTH;
use serde::Deserialize;
use tokio::time::Instant;

pub const OPENAI_FILE_URI_PREFIX: &str = "sediment://";
pub const OPENAI_FILE_UPLOAD_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

const OPENAI_FILE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Slowest sustained blob-upload rate still treated as progress. The upload's
/// whole-request timeout grows with the file so files up to the size limit remain
/// uploadable on ordinary uplinks while a stalled transfer is still bounded.
const OPENAI_FILE_UPLOAD_MIN_BYTES_PER_SECOND: u64 = 256 * 1024;
const OPENAI_FILE_FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const OPENAI_FILE_FINALIZE_RETRY_DELAY: Duration = Duration::from_millis(250);
const OPENAI_FILE_BLOB_MAX_RETRIES: u32 = 4;
const OPENAI_FILE_BLOB_RETRY_DELAY: Duration = Duration::from_millis(250);
const OPENAI_FILE_USE_CASE: &str = "codex";

enum OpenAiFileContents {
    Stream(Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>),
    Replayable(Bytes),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedOpenAiFile {
    pub file_id: String,
    pub uri: String,
    pub download_url: String,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub mime_type: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenAiFileError {
    #[error("OpenAI file base URL must be an absolute HTTP(S) URL")]
    InvalidBaseUrl,
    #[error("failed to authenticate OpenAI file request to {url}: {source}")]
    Auth {
        url: String,
        #[source]
        source: AuthError,
    },
    #[error(
        "file `{file_name}` is too large: {size_bytes} bytes exceeds the limit of {limit_bytes} bytes"
    )]
    FileTooLarge {
        file_name: String,
        size_bytes: u64,
        limit_bytes: u64,
    },
    #[error("failed to send OpenAI file request to {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: RouteAwareRequestError,
    },
    #[error("OpenAI file request to {url} failed with status {status}: {body}")]
    UnexpectedStatus {
        url: String,
        status: StatusCode,
        body: String,
    },
    #[error("failed to parse OpenAI file response from {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to build OpenAI file client for {url}: {source}")]
    ClientBuild {
        url: String,
        #[source]
        source: BuildRouteAwareHttpClientError,
    },
    #[error("OpenAI file upload for `{file_id}` is not ready yet")]
    UploadNotReady { file_id: String },
    #[error("OpenAI file upload for `{file_id}` failed: {message}")]
    UploadFailed { file_id: String, message: String },
    #[error(
        "{source}; additionally failed to delete remote file `{file_id}` during rollback: {rollback}"
    )]
    RollbackFailed {
        file_id: String,
        #[source]
        source: Box<OpenAiFileError>,
        rollback: Box<OpenAiFileError>,
    },
}

#[derive(Deserialize)]
struct CreateFileResponse {
    file_id: String,
    upload_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct DownloadLinkResponse {
    status: String,
    download_url: Option<String>,
    file_name: Option<String>,
    mime_type: Option<String>,
    error_message: Option<String>,
}

pub fn openai_file_uri(file_id: &str) -> String {
    format!("{OPENAI_FILE_URI_PREFIX}{file_id}")
}

/// Deletes a previously created OpenAI file.
///
/// This is intentionally idempotent so callers can use it for best-effort rollback after a
/// multi-file upload fails partway through.
pub async fn delete_openai_file(
    base_url: &str,
    auth: &dyn AuthProvider,
    http_client_factory: &HttpClientFactory,
    file_id: &str,
) -> Result<(), OpenAiFileError> {
    let http_clients = openai_file_http_client_pool(http_client_factory);
    delete_openai_file_with_pool(base_url, auth, &http_clients, file_id).await
}

pub async fn delete_openai_file_with_pool(
    base_url: &str,
    auth: &dyn AuthProvider,
    http_clients: &RouteAwareClientPool,
    file_id: &str,
) -> Result<(), OpenAiFileError> {
    let delete_url = file_endpoint_url(base_url, Some(file_id), false)?;
    let request = authorized_request(http_clients, auth,
        codex_client::Request::new(Method::DELETE, delete_url.clone())).await?;
    let response = codex_http_client::with_dispatch_guard(auth.dispatch_guard(), request.send()).await
        .map_err(|source| request_error(&delete_url, source))?;
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_FOUND {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(OpenAiFileError::UnexpectedStatus {
        url: delete_url,
        status,
        body,
    })
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(&mut encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn file_endpoint_url(base_url: &str, file_id: Option<&str>, uploaded: bool) -> Result<String, OpenAiFileError> {
    let mut url = url::Url::parse(base_url).map_err(|_| OpenAiFileError::InvalidBaseUrl)?;
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
        return Err(OpenAiFileError::InvalidBaseUrl);
    }
    let mut path = format!("{}/files", url.path().trim_end_matches('/'));
    if let Some(file_id) = file_id {
        if matches!(file_id, "." | "..") {
            return Err(OpenAiFileError::UploadFailed { file_id: file_id.into(), message: "invalid dot-segment file ID".into() });
        }
        path.push('/');
        path.push_str(&percent_encode_path_segment(file_id));
    }
    if uploaded { path.push_str("/uploaded"); }
    url.set_path(&path);
    Ok(url.to_string())
}

pub async fn upload_openai_file(
    base_url: &str,
    auth: &dyn AuthProvider,
    http_client_factory: &HttpClientFactory,
    file_name: String,
    file_size_bytes: u64,
    contents: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
) -> Result<UploadedOpenAiFile, OpenAiFileError> {
    let http_clients = openai_file_http_client_pool(http_client_factory);
    upload_openai_file_with_pool(
        base_url,
        auth,
        &http_clients,
        file_name,
        file_size_bytes,
        contents,
    )
    .await
}

pub async fn upload_openai_file_with_pool(
    base_url: &str,
    auth: &dyn AuthProvider,
    http_clients: &RouteAwareClientPool,
    file_name: String,
    file_size_bytes: u64,
    contents: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
) -> Result<UploadedOpenAiFile, OpenAiFileError> {
    upload_openai_file_with_pool_and_finalize_timeout(
        base_url,
        auth,
        http_clients,
        file_name,
        file_size_bytes,
        OpenAiFileContents::Stream(Box::pin(contents)),
        OPENAI_FILE_FINALIZE_TIMEOUT,
    )
    .await
}

/// Uploads immutable staged bytes, allowing bounded retries of the same blob PUT.
/// File allocation and finalization are never replayed by the blob retry loop.
pub async fn upload_openai_file_bytes_with_pool(
    base_url: &str,
    auth: &dyn AuthProvider,
    http_clients: &RouteAwareClientPool,
    file_name: String,
    contents: Bytes,
) -> Result<UploadedOpenAiFile, OpenAiFileError> {
    upload_openai_file_with_pool_and_finalize_timeout(
        base_url,
        auth,
        http_clients,
        file_name,
        u64::try_from(contents.len()).unwrap_or(u64::MAX),
        OpenAiFileContents::Replayable(contents),
        OPENAI_FILE_FINALIZE_TIMEOUT,
    )
    .await
}

async fn upload_openai_file_with_pool_and_finalize_timeout(
    base_url: &str,
    auth: &dyn AuthProvider,
    http_clients: &RouteAwareClientPool,
    file_name: String,
    file_size_bytes: u64,
    contents: OpenAiFileContents,
    finalize_timeout: Duration,
) -> Result<UploadedOpenAiFile, OpenAiFileError> {
    if file_size_bytes > OPENAI_FILE_UPLOAD_LIMIT_BYTES {
        return Err(OpenAiFileError::FileTooLarge {
            file_name,
            size_bytes: file_size_bytes,
            limit_bytes: OPENAI_FILE_UPLOAD_LIMIT_BYTES,
        });
    }

    let create_url = file_endpoint_url(base_url, None, false)?;
    let create_request = codex_client::Request::new(Method::POST, create_url.clone())
        .with_json(&serde_json::json!({
            "file_name": file_name.as_str(),
            "file_size": file_size_bytes,
            "use_case": OPENAI_FILE_USE_CASE,
        }));
    let create_request = authorized_request(http_clients, auth, create_request).await?;
    let create_response = codex_http_client::with_dispatch_guard(auth.dispatch_guard(), create_request.send()).await
        .map_err(|source| request_error(&create_url, source))?;
    let create_status = create_response.status();
    let create_body = create_response.text().await;
    if !create_status.is_success() {
        return Err(OpenAiFileError::UnexpectedStatus {
            url: create_url,
            status: create_status,
            body: create_body.unwrap_or_default(),
        });
    }
    let create_body = create_body.map_err(|source| request_error(&create_url, source.into()))?;
    let create_payload: CreateFileResponse =
        serde_json::from_str(&create_body).map_err(|source| OpenAiFileError::Decode {
            url: create_url.clone(),
            source,
        })?;
    let file_id = create_payload.file_id.clone();
    let upload_result: Result<UploadedOpenAiFile, OpenAiFileError> = async {
        let upload_timeout = openai_file_upload_timeout(file_size_bytes);
        let upload_deadline = Instant::now() + upload_timeout;
        let request = http_clients
            .request(Method::PUT, &create_payload.upload_url)
            .timeout(upload_timeout)
            .header("x-ms-blob-type", "BlockBlob")
            .header(CONTENT_LENGTH, file_size_bytes);
        let (mut request, replayable_contents) = match contents {
            OpenAiFileContents::Stream(contents) => (request.body_stream(contents), None),
            OpenAiFileContents::Replayable(contents) => {
                (request.body(contents.clone()), Some(contents))
            }
        };
        let mut retries = 0;
        loop {
            let (error, retryable, server_advice) = match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let server_advice = blob_upload_retry_after(response.headers());
                    let body = response.text().await.unwrap_or_default();
                    if status.is_success() {
                        break;
                    }
                    (OpenAiFileError::UnexpectedStatus {
                        url: diagnostic_url(&create_payload.upload_url),
                        status,
                        body,
                    }, matches!(status, StatusCode::BAD_GATEWAY
                        | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT), server_advice)
                }
                Err(source) => {
                    let retryable = source.is_timeout() || source.is_connect()
                        || source.is_body() || source.is_request();
                    (request_error(&create_payload.upload_url, source), retryable, None)
                }
            };
            let retry_at = server_advice.map(codex_http_client::RetryAfter::deadline)
                .unwrap_or_else(|| Instant::now()
                    + OPENAI_FILE_BLOB_RETRY_DELAY.saturating_mul(1 << retries));
            if retryable
                && retries < OPENAI_FILE_BLOB_MAX_RETRIES
                && let Some(contents) = replayable_contents.as_ref()
                && retry_at < upload_deadline
            {
                tokio::time::sleep_until(retry_at).await;
                if let Some(remaining) = upload_deadline.checked_duration_since(Instant::now()) {
                    request = http_clients
                        .request(Method::PUT, &create_payload.upload_url)
                        .timeout(remaining)
                        .header("x-ms-blob-type", "BlockBlob")
                        .header(CONTENT_LENGTH, file_size_bytes)
                        .body(contents.clone());
                    retries += 1;
                    continue;
                }
            }
            return Err(error);
        }

        let finalize_url = file_endpoint_url(base_url, Some(&create_payload.file_id), true)?;
        let finalize_deadline = Instant::now() + finalize_timeout;
        loop {
            let remaining = finalize_deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| OpenAiFileError::UploadNotReady {
                    file_id: create_payload.file_id.clone(),
                })?;
            let finalize_attempt = async {
                let mut request = codex_client::Request::new(Method::POST, finalize_url.clone())
                    .with_json(&serde_json::json!({}));
                request.timeout = Some(remaining);
                let request = authorized_request(http_clients, auth, request).await?;
                let response = codex_http_client::with_dispatch_guard(auth.dispatch_guard(), request.send()).await
                    .map_err(|source| request_error(&finalize_url, source))?;
                let status = response.status();
                let body = response.text().await;
                let body = if status.is_success() {
                    body.map_err(|source| request_error(&finalize_url, source.into()))?
                } else {
                    body.unwrap_or_default()
                };
                Ok::<_, OpenAiFileError>((status, body))
            };
            let (finalize_status, finalize_body) =
                match tokio::time::timeout_at(finalize_deadline, finalize_attempt).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(OpenAiFileError::Request { source, .. }))
                        if source.is_timeout() && Instant::now() >= finalize_deadline =>
                    {
                        return Err(OpenAiFileError::UploadNotReady {
                            file_id: create_payload.file_id.clone(),
                        });
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(OpenAiFileError::UploadNotReady {
                            file_id: create_payload.file_id.clone(),
                        });
                    }
                };
            if !finalize_status.is_success() {
                return Err(OpenAiFileError::UnexpectedStatus {
                    url: finalize_url.clone(),
                    status: finalize_status,
                    body: finalize_body,
                });
            }
            let finalize_payload: DownloadLinkResponse = serde_json::from_str(&finalize_body)
                .map_err(|source| OpenAiFileError::Decode {
                    url: finalize_url.clone(),
                    source,
                })?;

            match finalize_payload.status.as_str() {
                "success" => {
                    return Ok(UploadedOpenAiFile {
                        file_id: create_payload.file_id.clone(),
                        uri: openai_file_uri(&create_payload.file_id),
                        download_url: finalize_payload.download_url.ok_or_else(|| {
                            OpenAiFileError::UploadFailed {
                                file_id: create_payload.file_id.clone(),
                                message: "missing download_url".to_string(),
                            }
                        })?,
                        file_name: finalize_payload.file_name.unwrap_or(file_name),
                        file_size_bytes,
                        mime_type: finalize_payload.mime_type,
                    });
                }
                "retry" => {
                    let retry_deadline =
                        (Instant::now() + OPENAI_FILE_FINALIZE_RETRY_DELAY).min(finalize_deadline);
                    tokio::time::sleep_until(retry_deadline).await;
                    if Instant::now() >= finalize_deadline {
                        return Err(OpenAiFileError::UploadNotReady {
                            file_id: create_payload.file_id.clone(),
                        });
                    }
                }
                _ => {
                    return Err(OpenAiFileError::UploadFailed {
                        file_id: create_payload.file_id.clone(),
                        message: finalize_payload
                            .error_message
                            .unwrap_or_else(|| "upload finalization returned an error".to_string()),
                    });
                }
            }
        }
    }
    .await;

    match upload_result {
        Ok(uploaded) => Ok(uploaded),
        Err(source) => {
            match delete_openai_file_with_pool(base_url, auth, http_clients, &file_id).await {
                Ok(()) => Err(source),
                Err(rollback) => Err(OpenAiFileError::RollbackFailed {
                    file_id,
                    source: Box::new(source),
                    rollback: Box::new(rollback),
                }),
            }
        }
    }
}

fn openai_file_upload_timeout(file_size_bytes: u64) -> Duration {
    OPENAI_FILE_REQUEST_TIMEOUT.saturating_add(Duration::from_secs(
        file_size_bytes.div_ceil(OPENAI_FILE_UPLOAD_MIN_BYTES_PER_SECOND),
    ))
}

fn blob_upload_retry_after(headers: &http::HeaderMap) -> Option<codex_http_client::RetryAfter> {
    headers.get("x-ms-retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|delay| codex_http_client::RetryAfter::from_delay(Duration::from_millis(delay)))
        .or_else(|| codex_http_client::RetryAfter::from_headers(headers))
}

async fn authorized_request(
    http_clients: &RouteAwareClientPool,
    auth: &dyn AuthProvider,
    mut request: codex_client::Request,
) -> Result<RouteAwareRequestBuilder, OpenAiFileError> {
    let url = diagnostic_url(&request.url);
    request.timeout = Some(request.timeout.unwrap_or(OPENAI_FILE_REQUEST_TIMEOUT));
    let request = request.into_prepared()
        .map_err(|message| OpenAiFileError::Auth { url: url.clone(), source: AuthError::Build(message) })?;
    let request = auth.apply_auth(request).await
        .map_err(|source| OpenAiFileError::Auth {
            url: url.clone(),
            source,
        })?;
    let prepared = request.prepare_body_for_send()
        .map_err(|message| OpenAiFileError::Auth { url, source: AuthError::Build(message) })?;
    let mut builder = http_clients.request(request.method.clone(), &request.url)
        .headers(prepared.headers.clone())
        .timeout(request.timeout.unwrap_or(OPENAI_FILE_REQUEST_TIMEOUT));
    if request.body.is_some() { builder = builder.body(prepared.body_bytes()); }
    Ok(builder)
}

pub fn openai_file_http_client_pool(
    http_client_factory: &HttpClientFactory,
) -> RouteAwareClientPool {
    RouteAwareClientPool::new_without_request_logging(
        http_client_factory.clone(),
        ClientRouteClass::Api,
    )
}

fn request_error(url: &str, source: RouteAwareRequestError) -> OpenAiFileError {
    let url = diagnostic_url(url);
    match source.without_url() {
        RouteAwareRequestError::Route(RouteAwareClientPoolError::Build(source)) => {
            OpenAiFileError::ClientBuild { url, source }
        }
        source => OpenAiFileError::Request { url, source },
    }
}

fn diagnostic_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return "<invalid URL>".to_string();
    };
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

