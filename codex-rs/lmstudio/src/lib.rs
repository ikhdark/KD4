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
    let (lmstudio_client, mut models) =
        LMStudioClient::try_from_provider_with_models(config).await?;

    if !models.iter().any(|m| m == model) {
        lmstudio_client.download_model(model).await?;
        models.push(model.to_owned());
    }

    let provider = &config.model_providers[codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID];
    codex_model_provider::store_discovered_local_models(
        codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID,
        provider,
        config.codex_home.to_path_buf(),
        models,
    )
    .await;

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

