use codex_backend_client::Client as BackendClient;
use codex_backend_client::ConfigBundleResponse;
use codex_backend_client::DeliveredTomlFragment;
use codex_backend_client::RequestError;
use codex_config::CloudConfigBundle;
use codex_config::CloudConfigFragment;
use codex_config::CloudConfigTomlBundle;
use codex_config::CloudRequirementsFragment;
use codex_config::CloudRequirementsTomlBundle;
use codex_http_client::HttpClientFactory;
use codex_login::CodexAuth;
use std::future::Future;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetryableFailureKind {
    Request { status_code: Option<u16> },
}

impl RetryableFailureKind {
    pub(crate) fn status_code(self) -> Option<u16> {
        match self {
            Self::Request { status_code } => status_code,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BundleRequestError {
    Retryable(RetryableFailureKind),
    Permanent {
        status_code: Option<u16>,
    },
    Unauthorized {
        status_code: Option<u16>,
        message: String,
    },
}

/// Retrieves one cloud config bundle from the backend.
///
/// Implementations should return the backend-selected bundle exactly as delivered and leave
/// validation, caching, and config/requirements parsing decisions to the service layer.
pub(crate) trait BundleClient: Send + Sync {
    fn get_bundle(
        &self,
        auth: &CodexAuth,
    ) -> impl Future<Output = Result<CloudConfigBundle, BundleRequestError>> + Send;
}

pub(crate) struct BackendBundleClient {
    /// Unauthenticated template; each request applies the current auth snapshot, so
    /// retries and unauthorized recovery reuse one transport pool.
    client: BackendClient,
}

impl BackendBundleClient {
    pub(crate) fn new(base_url: String, http_client_factory: HttpClientFactory) -> Self {
        Self {
            client: BackendClient::new(base_url, http_client_factory),
        }
    }
}

impl BundleClient for BackendBundleClient {
    async fn get_bundle(&self, auth: &CodexAuth) -> Result<CloudConfigBundle, BundleRequestError> {
        let response = self
            .client
            .clone()
            .with_auth(auth)
            .get_config_bundle()
            .await
            .inspect_err(|err| {
                tracing::warn!(error = %err, "Failed to fetch cloud config bundle");
            })
            .map_err(classify_request_error)?;

        Ok(bundle_from_response(response))
    }
}

pub(crate) fn bundle_from_response(response: ConfigBundleResponse) -> CloudConfigBundle {
    let config_toml = response
        .config_toml
        .flatten()
        .map(|config_toml| *config_toml)
        .and_then(|config_toml| config_toml.enterprise_managed.flatten())
        .unwrap_or_default()
        .into_iter()
        .map(config_fragment_from_delivered)
        .collect();
    let requirements_toml = response
        .requirements_toml
        .flatten()
        .map(|requirements_toml| *requirements_toml)
        .and_then(|requirements_toml| requirements_toml.enterprise_managed.flatten())
        .unwrap_or_default()
        .into_iter()
        .map(requirements_fragment_from_delivered)
        .collect();

    CloudConfigBundle {
        config_toml: CloudConfigTomlBundle {
            enterprise_managed: config_toml,
        },
        requirements_toml: CloudRequirementsTomlBundle {
            enterprise_managed: requirements_toml,
        },
    }
}

fn config_fragment_from_delivered(fragment: DeliveredTomlFragment) -> CloudConfigFragment {
    CloudConfigFragment {
        id: fragment.id,
        name: fragment.name,
        contents: fragment.contents,
    }
}

fn requirements_fragment_from_delivered(
    fragment: DeliveredTomlFragment,
) -> CloudRequirementsFragment {
    CloudRequirementsFragment {
        id: fragment.id,
        name: fragment.name,
        contents: fragment.contents,
    }
}

fn classify_request_error(err: RequestError) -> BundleRequestError {
    let status_code = err.status().map(|status| status.as_u16());
    if err.is_unauthorized() {
        return BundleRequestError::Unauthorized {
            status_code,
            message: err.to_string(),
        };
    }
    // The endpoint returns structured route/transport errors from send(), and
    // Other errors from JSON decoding. Do not retry deterministic decoding errors.
    let retryable = match &err {
        RequestError::UnexpectedStatus { status, .. } => {
            matches!(status.as_u16(), 408 | 429) || status.is_server_error()
        }
        RequestError::Other(err) => err
            .downcast_ref::<codex_http_client::RouteAwareRequestError>()
            .is_some(),
    };
    if retryable {
        BundleRequestError::Retryable(RetryableFailureKind::Request { status_code })
    } else {
        BundleRequestError::Permanent { status_code }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_status_and_decode_failures() {
        for status in [400u16, 401, 403, 404, 408, 422, 429, 500, 503] {
            let err = RequestError::UnexpectedStatus {
                method: "GET".into(),
                url: "/config/bundle".into(),
                status: status.try_into().expect("status"),
                content_type: "application/json".into(),
                body: String::new(),
            };
            let result = classify_request_error(err);
            match status {
                401 => assert!(matches!(
                    result,
                    BundleRequestError::Unauthorized {
                        status_code: Some(401),
                        ..
                    }
                )),
                408 | 429 | 500 | 503 => assert_eq!(
                    result,
                    BundleRequestError::Retryable(RetryableFailureKind::Request {
                        status_code: Some(status)
                    })
                ),
                _ => assert_eq!(
                    result,
                    BundleRequestError::Permanent {
                        status_code: Some(status)
                    }
                ),
            }
        }
        let decode_error =
            serde_json::from_str::<ConfigBundleResponse>("{").expect_err("invalid JSON");
        assert_eq!(
            classify_request_error(RequestError::Other(decode_error.into())),
            BundleRequestError::Permanent { status_code: None }
        );
    }

    #[tokio::test]
    async fn requests_apply_each_auth_snapshot_over_one_connection() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use tokio::io::AsyncBufReadExt;
        use tokio::io::AsyncWriteExt;
        use tokio::io::BufReader;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let connections = Arc::new(AtomicUsize::new(0));
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let connections = Arc::clone(&connections);
            let authorizations = Arc::clone(&authorizations);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    connections.fetch_add(1, Ordering::SeqCst);
                    let authorizations = Arc::clone(&authorizations);
                    tokio::spawn(async move {
                        let mut stream = BufReader::new(stream);
                        // Serve keep-alive requests until the client closes the connection.
                        loop {
                            let mut authorization = None;
                            loop {
                                let mut line = String::new();
                                if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                                    return;
                                }
                                if line == "\r\n" {
                                    break;
                                }
                                if let Some((name, value)) = line.split_once(':')
                                    && name.eq_ignore_ascii_case("authorization")
                                {
                                    authorization = Some(value.trim().to_string());
                                }
                            }
                            authorizations
                                .lock()
                                .expect("authorizations lock")
                                .push(authorization);
                            let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
                            if stream
                                .get_mut()
                                .write_all(response.as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    });
                }
            })
        };

        let client = BackendBundleClient::new(
            format!("http://{address}"),
            HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault),
        );
        for key in ["sk-first", "sk-second"] {
            assert_eq!(
                client.get_bundle(&CodexAuth::from_api_key(key)).await,
                Ok(CloudConfigBundle::default())
            );
        }
        server.abort();

        assert_eq!(
            *authorizations.lock().expect("authorizations lock"),
            vec![
                Some("Bearer sk-first".to_string()),
                Some("Bearer sk-second".to_string())
            ]
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }
}
