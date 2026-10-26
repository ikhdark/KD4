//! OSS provider utilities shared between TUI and exec.

use codex_core::config::Config;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;

/// Returns the default model for a given OSS provider.
pub fn get_default_model_for_oss_provider(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        LMSTUDIO_OSS_PROVIDER_ID => Some(codex_lmstudio::DEFAULT_OSS_MODEL),
        OLLAMA_OSS_PROVIDER_ID => Some(codex_ollama::DEFAULT_OSS_MODEL),
        _ => None,
    }
}

/// Runs the selected provider's setup, preserving its I/O error kind.
/// LM Studio warm-up is best-effort and may still be running on return.
pub async fn ensure_oss_provider_ready(
    provider_id: &str,
    config: &Config,
) -> Result<(), std::io::Error> {
    match provider_id {
        LMSTUDIO_OSS_PROVIDER_ID => {
            codex_lmstudio::ensure_oss_ready(config)
                .await
                .map_err(oss_setup_error)?;
        }
        OLLAMA_OSS_PROVIDER_ID => {
            codex_ollama::ensure_oss_ready(config)
                .await
                .map_err(oss_setup_error)?;
        }
        _ => {
            // Unknown provider, skip setup
        }
    }
    Ok(())
}

fn oss_setup_error(error: std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("OSS setup failed: {error}"))
}

