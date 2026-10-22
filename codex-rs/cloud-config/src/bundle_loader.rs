use crate::backend::BackendBundleClient;
use crate::service::CLOUD_CONFIG_BUNDLE_TIMEOUT;
use crate::service::CloudConfigBundleService;
use codex_config::CloudConfigBundleLoadError;
use codex_config::CloudConfigBundleLoadErrorCode;
use codex_config::CloudConfigBundleLoader;
use codex_config::types::AuthCredentialsStoreMode;
use codex_http_client::HttpClientFactory;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use std::path::PathBuf;
use std::sync::Arc;

pub fn cloud_config_bundle_loader(
    auth_manager: Arc<AuthManager>,
    chatgpt_base_url: String,
    codex_home: PathBuf,
    http_client_factory: HttpClientFactory,
) -> CloudConfigBundleLoader {
    let service = CloudConfigBundleService::new(
        auth_manager,
        Arc::new(BackendBundleClient::new(
            chatgpt_base_url,
            http_client_factory,
        )),
        codex_home,
        CLOUD_CONFIG_BUNDLE_TIMEOUT,
    );
    retryable_service_loader(service)
}

pub(crate) fn retryable_service_loader<C: crate::backend::BundleClient + 'static>(
    service: CloudConfigBundleService<C>,
) -> CloudConfigBundleLoader {
    let validity_service = service.clone();
    CloudConfigBundleLoader::retryable(move || {
        let service = service.clone();
        // Spawn at attempt creation, not first polling: admitted work remains
        // owned even if its first observer goes away.
        let task = tokio::spawn(async move { service.load_startup_bundle_with_timeout().await });
        async move {
            task.await.map_err(|err| {
                tracing::error!(error = %err, "Cloud config bundle task failed");
                CloudConfigBundleLoadError::new(
                    CloudConfigBundleLoadErrorCode::Internal,
                    /*status_code*/ None,
                    format!("cloud config bundle load failed: {err}"),
                )
            })?
        }
    })
    .with_publication_guard(move |publish| validity_service.publish_if_current(publish))
}

pub async fn cloud_config_bundle_loader_for_storage(
    codex_home: PathBuf,
    enable_codex_api_key_env: bool,
    credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    chatgpt_base_url: String,
    auth_route_config: AuthRouteConfig,
) -> CloudConfigBundleLoader {
    let http_client_factory = auth_route_config.http_client_factory().clone();
    let auth_manager = AuthManager::shared(
        codex_home.clone(),
        enable_codex_api_key_env,
        credentials_store_mode,
        /*forced_chatgpt_workspace_id*/ None,
        Some(chatgpt_base_url.clone()),
        keyring_backend_kind,
        auth_route_config,
    )
    .await;
    cloud_config_bundle_loader(
        auth_manager,
        chatgpt_base_url,
        codex_home,
        http_client_factory,
    )
}
