use std::time::Duration;

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
const OPENAI_FILE_FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const OPENAI_FILE_FINALIZE_RETRY_DELAY: Duration = Duration::from_millis(250);
const OPENAI_FILE_USE_CASE: &str = "codex";

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
    let encoded_file_id = percent_encode_path_segment(file_id);
    let delete_url = format!("{}/files/{encoded_file_id}", base_url.trim_end_matches('/'));
    let response = authorized_request(http_clients, auth, Method::DELETE, &delete_url)
        .send()
        .await
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
        contents,
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
    contents: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
    finalize_timeout: Duration,
) -> Result<UploadedOpenAiFile, OpenAiFileError> {
    if file_size_bytes > OPENAI_FILE_UPLOAD_LIMIT_BYTES {
        return Err(OpenAiFileError::FileTooLarge {
            file_name,
            size_bytes: file_size_bytes,
            limit_bytes: OPENAI_FILE_UPLOAD_LIMIT_BYTES,
        });
    }

    let create_url = format!("{}/files", base_url.trim_end_matches('/'));
    let create_response = authorized_request(http_clients, auth, Method::POST, &create_url)
        .json(&serde_json::json!({
            "file_name": file_name.as_str(),
            "file_size": file_size_bytes,
            "use_case": OPENAI_FILE_USE_CASE,
        }))
        .send()
        .await
        .map_err(|source| request_error(&create_url, source))?;
    let create_status = create_response.status();
    let create_body = create_response.text().await.unwrap_or_default();
    if !create_status.is_success() {
        return Err(OpenAiFileError::UnexpectedStatus {
            url: create_url,
            status: create_status,
            body: create_body,
        });
    }
    let create_payload: CreateFileResponse =
        serde_json::from_str(&create_body).map_err(|source| OpenAiFileError::Decode {
            url: create_url.clone(),
            source,
        })?;
    let file_id = create_payload.file_id.clone();
    let upload_result: Result<UploadedOpenAiFile, OpenAiFileError> = async {
        let upload_response = http_clients
            .request(Method::PUT, &create_payload.upload_url)
            .timeout(OPENAI_FILE_REQUEST_TIMEOUT)
            .header("x-ms-blob-type", "BlockBlob")
            .header(CONTENT_LENGTH, file_size_bytes)
            .body_stream(contents)
            .send()
            .await
            .map_err(|source| request_error(&create_payload.upload_url, source.without_url()))?;
        let upload_status = upload_response.status();
        let upload_body = upload_response.text().await.unwrap_or_default();
        if !upload_status.is_success() {
            return Err(OpenAiFileError::UnexpectedStatus {
                url: create_payload.upload_url.clone(),
                status: upload_status,
                body: upload_body,
            });
        }

        let finalize_url = format!(
            "{}/files/{}/uploaded",
            base_url.trim_end_matches('/'),
            create_payload.file_id,
        );
        let finalize_deadline = Instant::now() + finalize_timeout;
        loop {
            let remaining = finalize_deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| OpenAiFileError::UploadNotReady {
                    file_id: create_payload.file_id.clone(),
                })?;
            let finalize_attempt = async {
                let response = authorized_request(http_clients, auth, Method::POST, &finalize_url)
                    .timeout(remaining)
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                    .map_err(|source| request_error(&finalize_url, source))?;
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
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

fn authorized_request(
    http_clients: &RouteAwareClientPool,
    auth: &dyn AuthProvider,
    method: Method,
    url: &str,
) -> RouteAwareRequestBuilder {
    let mut headers = http::HeaderMap::new();
    auth.add_auth_headers(&mut headers);

    http_clients
        .request(method, url)
        .timeout(OPENAI_FILE_REQUEST_TIMEOUT)
        .headers(headers)
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
    match source {
        RouteAwareRequestError::Route(RouteAwareClientPoolError::Build(source)) => {
            OpenAiFileError::ClientBuild {
                url: url.to_string(),
                source,
            }
        }
        source => OpenAiFileError::Request {
            url: url.to_string(),
            source,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::OutboundProxyPolicy;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::Request;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_json;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[derive(Clone, Copy)]
    struct ChatGptTestAuth;

    fn default_http_client_factory() -> HttpClientFactory {
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
    }

    impl AuthProvider for ChatGptTestAuth {
        fn add_auth_headers(&self, headers: &mut http::HeaderMap) {
            headers.insert(
                http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer token"),
            );
            headers.insert("ChatGPT-Account-ID", HeaderValue::from_static("account_id"));
        }
    }

    fn chatgpt_auth() -> ChatGptTestAuth {
        ChatGptTestAuth
    }

    fn base_url_for(server: &MockServer) -> String {
        format!("{}/backend-api", server.uri())
    }

    #[tokio::test]
    async fn invalid_custom_ca_is_rejected_for_every_proxy_policy() {
        const CHILD_POLICY_ENV: &str = "CODEX_API_FILES_INVALID_CA_TEST_POLICY";

        let Ok(policy_name) = std::env::var(CHILD_POLICY_ENV) else {
            let unique_suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after the Unix epoch")
                .as_nanos();
            let invalid_ca_path = std::env::temp_dir().join(format!(
                "codex-api-invalid-ca-{}-{unique_suffix}.pem",
                std::process::id()
            ));
            std::fs::write(&invalid_ca_path, "not a PEM certificate")
                .expect("invalid CA fixture should be written");

            for ca_env in ["CODEX_CA_CERTIFICATE", "SSL_CERT_FILE"] {
                for policy_name in ["reqwest-default", "respect-system-proxy"] {
                    let output = std::process::Command::new(
                        std::env::current_exe().expect("test executable should be available"),
                    )
                    .arg("--exact")
                    .arg("files::tests::invalid_custom_ca_is_rejected_for_every_proxy_policy")
                    .arg("--nocapture")
                    .env_remove("CODEX_CA_CERTIFICATE")
                    .env_remove("SSL_CERT_FILE")
                    .env(ca_env, &invalid_ca_path)
                    .env(CHILD_POLICY_ENV, policy_name)
                    .output()
                    .expect("isolated CA subprocess should run");

                    assert!(
                        output.status.success(),
                        "{policy_name} failed with invalid {ca_env}\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
            }
            std::fs::remove_file(&invalid_ca_path).expect("invalid CA fixture should be removed");
            return;
        };

        let outbound_proxy_policy = match policy_name.as_str() {
            "reqwest-default" => OutboundProxyPolicy::ReqwestDefault,
            "respect-system-proxy" => OutboundProxyPolicy::RespectSystemProxy,
            _ => panic!("unexpected test proxy policy: {policy_name}"),
        };
        let http_clients =
            openai_file_http_client_pool(&HttpClientFactory::new(outbound_proxy_policy));
        let error = http_clients
            .get("https://example.com/upload")
            .send()
            .await
            .expect_err("file uploads should reject invalid custom CAs");
        let error = request_error("https://example.com/upload", error);

        assert!(matches!(error, OpenAiFileError::ClientBuild { .. }));
    }

    #[tokio::test]
    async fn delete_openai_file_encodes_the_id_and_accepts_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/backend-api/files/file%2Fwith%20spaces"))
            .and(header("chatgpt-account-id", "account_id"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        delete_openai_file(
            &base_url_for(&server),
            &chatgpt_auth(),
            &default_http_client_factory(),
            "file/with spaces",
        )
        .await
        .expect("not-found deletion is idempotent");
    }

    #[tokio::test]
    async fn upload_openai_file_reuses_operation_pool_across_finalize_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .and(header("chatgpt-account-id", "account_id"))
            .and(body_json(serde_json::json!({
                "file_name": "hello.txt",
                "file_size": 5,
                "use_case": "codex",
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"file_id": "file_123", "upload_url": format!("{}/upload/file_123", server.uri())})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_123"))
            .and(header("content-length", "5"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let finalize_attempts = Arc::new(AtomicUsize::new(0));
        let finalize_attempts_responder = Arc::clone(&finalize_attempts);
        let download_url = format!("{}/download/file_123", server.uri());
        Mock::given(method("POST"))
            .and(path("/backend-api/files/file_123/uploaded"))
            .respond_with(move |_request: &Request| {
                if finalize_attempts_responder.fetch_add(1, Ordering::SeqCst) == 0 {
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "status": "retry"
                    }));
                }

                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "status": "success",
                    "download_url": download_url,
                    "file_name": "hello.txt",
                    "mime_type": "text/plain",
                    "file_size_bytes": 5
                }))
            })
            .mount(&server)
            .await;

        let base_url = base_url_for(&server);
        let contents =
            futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"hello"))]);
        let http_clients = openai_file_http_client_pool(&default_http_client_factory());
        let uploaded = upload_openai_file_with_pool(
            &base_url,
            &chatgpt_auth(),
            &http_clients,
            "hello.txt".to_string(),
            /*file_size_bytes*/ 5,
            contents,
        )
        .await
        .expect("upload succeeds");

        assert_eq!(uploaded.file_id, "file_123");
        assert_eq!(uploaded.uri, "sediment://file_123");
        assert_eq!(
            uploaded.download_url,
            format!("{}/download/file_123", server.uri())
        );
        assert_eq!(uploaded.file_name, "hello.txt");
        assert_eq!(uploaded.mime_type, Some("text/plain".to_string()));
        assert_eq!(finalize_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            http_clients.cached_route_count(),
            1,
            "create, upload, and finalize should share one resolved-route client"
        );
    }

    #[tokio::test]
    async fn finalize_in_flight_request_is_bounded_by_the_overall_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_id": "file_deadline",
                "upload_url": format!("{}/upload/file_deadline", server.uri()),
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_deadline"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let finalize_attempts = Arc::new(AtomicUsize::new(0));
        let observed_finalize_attempts = Arc::clone(&finalize_attempts);
        Mock::given(method("POST"))
            .and(path("/backend-api/files/file_deadline/uploaded"))
            .respond_with(move |_request: &Request| {
                observed_finalize_attempts.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .set_delay(OPENAI_FILE_REQUEST_TIMEOUT)
                    .set_body_json(serde_json::json!({"status": "retry"}))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/backend-api/files/file_deadline"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let http_clients = openai_file_http_client_pool(&default_http_client_factory());
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            upload_openai_file_with_pool_and_finalize_timeout(
                &base_url_for(&server),
                &chatgpt_auth(),
                &http_clients,
                "deadline.txt".to_string(),
                5,
                futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"hello"))]),
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("the overall finalization deadline should bound the in-flight request")
        .expect_err("finalization should hit the overall deadline");
        assert!(
            matches!(error, OpenAiFileError::UploadNotReady { .. }),
            "unexpected finalization error: {error:?}"
        );
        assert_eq!(finalize_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn upload_openai_file_deletes_the_created_file_when_upload_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_id": "file_rollback",
                "upload_url": format!("{}/upload/file_rollback", server.uri()),
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/file_rollback"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upload failed"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/backend-api/files/file_rollback"))
            .and(header("chatgpt-account-id", "account_id"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let contents =
            futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"hello"))]);
        let error = upload_openai_file(
            &base_url_for(&server),
            &chatgpt_auth(),
            &default_http_client_factory(),
            "hello.txt".to_string(),
            /*file_size_bytes*/ 5,
            contents,
        )
        .await
        .expect_err("the failed upload must be reported");

        assert!(error.to_string().contains("500 Internal Server Error"));
    }
}
