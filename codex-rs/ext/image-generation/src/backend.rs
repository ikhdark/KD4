use codex_api::ImageEditRequest;
use codex_api::ImageGenerationRequest;
use codex_api::ImageResponse;
use codex_api::ImagesClient;
use codex_api::ReqwestTransport;
use codex_http_client::RouteAwareClientPool;
use codex_model_provider::SharedModelProvider;
use http::HeaderMap;
use std::sync::Arc;

struct ImageRequestTelemetry {
    span: tracing::Span,
}

impl codex_api::RequestTelemetry for ImageRequestTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<http::StatusCode>,
        error: Option<&codex_api::TransportError>,
        duration: std::time::Duration,
    ) {
        self.span.in_scope(|| {
            tracing::info!(
                event.name = "codex.image_api_request",
                attempt,
                status = status.map(|status| status.as_u16()),
                failed = error.is_some(),
                headers_duration_ms = duration.as_millis().min(u128::from(u64::MAX)) as u64,
                "standalone image HTTP attempt settled"
            )
        });
    }
}

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
        let (provider, auth) = self
            .provider
            .resolve_unscoped_client_setup()
            .await
            .map_err(|err| err.to_string())?;
        let endpoint = provider.url_for_path(path);
        self.http_clients
            .client_for_url(&endpoint)
            .await
            .map_err(|err| err.to_string())?;
        Ok(ImagesClient::new(
            ReqwestTransport::from_client_pool(self.http_clients.clone()),
            provider,
            auth,
        )
        .with_telemetry(Some(Arc::new(ImageRequestTelemetry {
            span: tracing::Span::current(),
        }))))
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
