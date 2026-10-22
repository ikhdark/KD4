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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::ClientRouteClass;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn image_generate_and_edit_keep_route_aware_redirect_dispatch() {
        core_test_support::require_network!();
        for (endpoint, status) in [("images/generations", 307), ("images/edits", 308)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(format!("/v1/{endpoint}")))
                .respond_with(
                    ResponseTemplate::new(status).insert_header("Location", "/v1/accepted-image"),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/v1/accepted-image"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "created":1,"data":[{"b64_json":"cG5n"}]
                })))
                .expect(1)
                .mount(&server)
                .await;
            let provider = codex_model_provider::create_model_provider(
                codex_model_provider_info::ModelProviderInfo {
                    base_url: Some(format!("{}/v1", server.uri())),
                    requires_openai_auth: false,
                    ..Default::default()
                },
                None,
            );
            let backend = CodexImagesBackend::new(
                provider,
                RouteAwareClientPool::new(
                    HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
                    ClientRouteClass::Api,
                ),
            );
            let response = if endpoint == "images/generations" {
                backend
                    .generate(ImageGenerationRequest {
                        prompt: "unchanged prompt".into(),
                        model: "image-fixture".into(),
                        background: None,
                        n: Some(1),
                        quality: None,
                        size: None,
                    })
                    .await
            } else {
                backend
                    .edit(ImageEditRequest {
                        images: Vec::new(),
                        prompt: "unchanged prompt".into(),
                        model: "image-fixture".into(),
                        background: None,
                        n: Some(1),
                        quality: None,
                        size: None,
                    })
                    .await
            };
            assert_eq!(response.unwrap().data.len(), 1);
            let requests = server.received_requests().await.unwrap();
            assert_eq!(
                requests.len(),
                2,
                "one redirect and one accepted generation"
            );
            assert_eq!(requests[0].body, requests[1].body);
        }
    }
}
