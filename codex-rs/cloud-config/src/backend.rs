use codex_backend_client::Client as BackendClient;
use codex_backend_client::ConfigBundleResponse;
use codex_backend_client::DeliveredTomlFragment;
use codex_config::CloudConfigBundle;
use codex_config::CloudConfigBundleLoadError;
use codex_config::CloudConfigBundleLoadErrorCode;
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
    RequestWithAdvice {
        status_code: Option<u16>,
        retry_after: codex_http_client::RetryAfter,
    },
}

impl RetryableFailureKind {
    pub(crate) fn status_code(self) -> Option<u16> {
        match self {
            Self::Request { status_code } | Self::RequestWithAdvice { status_code, .. } => status_code,
        }
    }

    pub(crate) fn retry_after(self) -> Option<codex_http_client::RetryAfter> {
        match self {
            Self::RequestWithAdvice { retry_after, .. } => Some(retry_after),
            Self::Request { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BundleRequestError {
    Retryable(RetryableFailureKind),
    Terminal(CloudConfigBundleLoadError),
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
                } else if matches!(err, codex_backend_client::RequestError::LocalAuth(_)) {
                    BundleRequestError::Terminal(CloudConfigBundleLoadError::new(
                        CloudConfigBundleLoadErrorCode::Auth,
                        status_code,
                        "failed to construct authenticated cloud config request",
                    ))
                } else if matches!(err, codex_backend_client::RequestError::InvalidResponse(_)) {
                    BundleRequestError::Terminal(CloudConfigBundleLoadError::new(
                        CloudConfigBundleLoadErrorCode::InvalidBundle,
                        status_code,
                        "invalid cloud config bundle response",
                    ))
                } else if matches!(
                    &err,
                    codex_backend_client::RequestError::IncompleteResponse { status, .. }
                        if status.is_success()
                ) {
                    // A success whose body was cut off is a transport failure,
                    // not a terminal HTTP status.
                    BundleRequestError::Retryable(RetryableFailureKind::Request {
                        status_code: None,
                    })
                } else if status_code.is_none_or(|status| {
                    status == 408 || status == 429 || (500..600).contains(&status)
                }) {
                    BundleRequestError::Retryable(match err.retry_after() {
                        Some(retry_after) => RetryableFailureKind::RequestWithAdvice {
                            status_code, retry_after,
                        },
                        None => RetryableFailureKind::Request { status_code },
                    })
                } else {
                    BundleRequestError::Terminal(CloudConfigBundleLoadError::new(
                        CloudConfigBundleLoadErrorCode::RequestFailed,
                        status_code,
                        "cloud config bundle request failed with a terminal HTTP status",
                    ))
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

