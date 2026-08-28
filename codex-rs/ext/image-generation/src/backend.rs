use codex_api::ImageEditRequest;
use codex_api::ImageGenerationRequest;
use codex_api::ImageResponse;
use codex_api::ImagesClient;
use codex_api::ReqwestTransport;
use codex_http_client::RouteAwareClientPool;
use codex_model_provider::SharedModelProvider;
use http::HeaderMap;

#[derive(Clone)]
pub(crate) struct CodexImagesBackend {
    provider: SharedModelProvider,
    http_clients: RouteAwareClientPool,
}

impl CodexImagesBackend {
    /// Creates a backend that sends image requests through the active model provider.
    pub(crate) fn new(provider: SharedModelProvider, http_clients: RouteAwareClientPool) -> Self {
        Self {
            provider,
            http_clients,
        }
    }

    /// Resolves the provider and auth required for the current image API request.
    async fn client(&self, path: &str) -> Result<ImagesClient<ReqwestTransport>, String> {
        let provider = self
            .provider
            .api_provider()
            .await
            .map_err(|err| err.to_string())?;
        let auth = self
            .provider
            .api_auth()
            .await
            .map_err(|err| err.to_string())?;
        let endpoint = provider.url_for_path(path);
        let client = self
            .http_clients
            .client_for_url(&endpoint)
            .await
            .map_err(|err| err.to_string())?;
        Ok(ImagesClient::new(
            ReqwestTransport::from_http_client(client),
            provider,
            auth,
        ))
    }

    /// Sends a standalone image generation request through the configured Images client.
    pub(crate) async fn generate(
        &self,
        request: ImageGenerationRequest,
    ) -> Result<ImageResponse, String> {
        self.client("images/generations")
            .await?
            .generate(&request, HeaderMap::new())
            .await
            .map_err(|err| err.to_string())
    }

    /// Sends a standalone image edit request through the configured Images client.
    pub(crate) async fn edit(&self, request: ImageEditRequest) -> Result<ImageResponse, String> {
        self.client("images/edits")
            .await?
            .edit(&request, HeaderMap::new())
            .await
            .map_err(|err| err.to_string())
    }
}
