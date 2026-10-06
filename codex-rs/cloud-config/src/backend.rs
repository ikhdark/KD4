use codex_backend_client::Client as BackendClient;
use codex_backend_client::ConfigBundleResponse;
use codex_backend_client::DeliveredTomlFragment;
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
        // Keep the route-aware transport pool across retries and refreshes, but
        // always use the current credentials (including after 401 recovery).
        let client = self.client.clone().with_auth(auth);

        let response = client
            .get_config_bundle()
            .await
            .inspect_err(|err| {
                tracing::warn!(error = %err, "Failed to fetch cloud config bundle");
            })
            .map_err(|err| {
                let status_code = err.status().map(|status| status.as_u16());
                if err.is_unauthorized() {
                    BundleRequestError::Unauthorized {
                        status_code,
                        message: err.to_string(),
                    }
                } else {
                    BundleRequestError::Retryable(RetryableFailureKind::Request { status_code })
                }
            })?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::OutboundProxyPolicy;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn bundle_requests_reuse_connection_with_fresh_auth_after_errors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let base_url = format!("http://{}", listener.local_addr().expect("address"));
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        // JoinSets cancel the accept loop and all connection workers on failure too.
        let mut server = tokio::task::JoinSet::new();
        server.spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            let request_count = Arc::new(AtomicUsize::new(0));
            let mut connection_id = 0;
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept");
                connection_id += 1;
                let requests_tx = requests_tx.clone();
                let request_count = Arc::clone(&request_count);
                connections.spawn(async move {
                    loop {
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            let Ok(byte) = stream.read_u8().await else {
                                return;
                            };
                            request.push(byte);
                            assert!(request.len() < 16 * 1024, "bounded request headers");
                        }
                        requests_tx
                            .send((connection_id, String::from_utf8(request).expect("headers")))
                            .expect("record request");
                        let status = match request_count.fetch_add(1, Ordering::SeqCst) {
                            0 => "503 Service Unavailable",
                            1 => "401 Unauthorized",
                            _ => "200 OK",
                        };
                        stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
                                )
                                .as_bytes(),
                            )
                            .await
                            .expect("response");
                    }
                });
            }
        });
        let client = BackendBundleClient::new(
            base_url,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        );
        let initial = CodexAuth::from_external_chatgpt_tokens(
            "e30.e30.initial",
            "account-a",
            Some("enterprise"),
        )
        .expect("initial auth");
        let refreshed = CodexAuth::from_external_chatgpt_tokens(
            "e30.e30.refreshed",
            "account-a",
            Some("enterprise"),
        )
        .expect("refreshed auth");
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            assert_eq!(
                client.get_bundle(&initial).await,
                Err(BundleRequestError::Retryable(
                    RetryableFailureKind::Request {
                        status_code: Some(503),
                    }
                ))
            );
            assert!(matches!(
                client.get_bundle(&initial).await,
                Err(BundleRequestError::Unauthorized {
                    status_code: Some(401),
                    ..
                })
            ));
            assert_eq!(
                client.get_bundle(&refreshed).await,
                Ok(CloudConfigBundle::default())
            );
            let mut connection_ids = Vec::new();
            for token in ["e30.e30.initial", "e30.e30.initial", "e30.e30.refreshed"] {
                let (id, request) = requests_rx.recv().await.expect("recorded request");
                connection_ids.push(id);
                assert!(request.starts_with("GET /api/codex/config/bundle HTTP/1.1\r\n"));
                let headers = request.to_ascii_lowercase();
                assert!(headers.contains(&format!("\r\nauthorization: bearer {token}\r\n")));
                assert!(headers.contains("\r\nchatgpt-account-id: account-a\r\n"));
            }
            connection_ids
        })
        .await;
        server.shutdown().await;
        let connection_ids = outcome.expect("requests finish within timeout");
        eprintln!("cloud bundle request connection IDs: {connection_ids:?}");
        assert_eq!(connection_ids, vec![1, 1, 1]);
    }
}
