mod client;

pub use client::LMStudioClient;
use codex_core::config::Config;

/// Default OSS model to use when `--oss` is passed without an explicit `-m`.
pub const DEFAULT_OSS_MODEL: &str = "openai/gpt-oss-20b";

/// Prepare the local OSS environment when `--oss` is selected.
///
/// - Ensures a local LM Studio server is reachable.
/// - Checks if the model exists locally and downloads it if missing.
/// - Starts best-effort model warm-up, which may still be running on return.
pub async fn ensure_oss_ready(config: &Config) -> std::io::Result<()> {
    let model = match config.model.as_ref() {
        Some(model) => model,
        None => DEFAULT_OSS_MODEL,
    };

    // Verify local LM Studio is reachable.
    let (lmstudio_client, models) = LMStudioClient::try_from_provider_with_models(config).await?;

    if !models.iter().any(|m| m == model) {
        lmstudio_client.download_model(model).await?;
    }

    // Load the model in the background
    tokio::spawn({
        let client = lmstudio_client.clone();
        let model = model.to_string();
        async move {
            if let Err(e) = client.load_model(&model).await {
                tracing::warn!("Failed to load model {}: {}", model, e);
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn malformed_catalogue_stops_setup_before_model_preparation() {
        // Include the selected model in malformed lists so even the old permissive
        // parser cannot start a real download during this regression test.
        for body in [
            json!({}),
            json!({"data": [{"id": "test-model"}, null]}),
            json!({"data": [{"id": "test-model"}, {}]}),
            json!({"data": [{"id": "test-model"}, {"id": 7}]}),
            json!({"data": [{"id": "test-model"}, {"id": "  "}]}),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let home = tempfile::tempdir().unwrap();
            let mut config = codex_core::config::ConfigBuilder::default()
                .codex_home(home.path().to_path_buf())
                .build()
                .await
                .unwrap();
            config.model = Some("test-model".into());
            config
                .model_providers
                .get_mut(LMSTUDIO_OSS_PROVIDER_ID)
                .unwrap()
                .base_url = Some(server.uri());
            let error = ensure_oss_ready(&config).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method.as_str(), "GET");
        }
    }
}
