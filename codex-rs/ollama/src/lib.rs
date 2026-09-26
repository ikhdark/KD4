mod client;
mod line_buffer;
mod parser;
mod pull;
mod url;

pub use client::OllamaClient;
use codex_core::config::Config;
pub use pull::PullEvent;
use semver::Version;

/// Default OSS model to use when `--oss` is passed without an explicit `-m`.
pub const DEFAULT_OSS_MODEL: &str = "gpt-oss:20b";

/// Prepare the local OSS environment when `--oss` is selected.
///
/// - Ensures a local Ollama server is reachable and supports the Responses API.
/// - Checks if the model exists locally and pulls it if missing.
pub async fn ensure_oss_ready(config: &Config) -> std::io::Result<()> {
    // Use the requested model, or the default OSS model when -m is not provided.
    let model = match config.model.as_ref() {
        Some(model) => model,
        None => DEFAULT_OSS_MODEL,
    };

    // Verify local Ollama is reachable; the same client serves every setup request.
    let ollama_client = crate::OllamaClient::try_from_oss_provider(config).await?;
    ensure_responses_supported(&ollama_client).await?;

    // If the model is not present locally, pull it. A failed listing is not evidence that the
    // model is missing, so it never starts a download.
    match ollama_client.fetch_models().await {
        Ok(models) => {
            if !models.iter().any(|listed| is_same_model(listed, model)) {
                ollama_client.pull_with_cli_progress(model).await?;
            }
        }
        Err(err) => {
            // Not fatal; higher layers may still proceed and surface errors later.
            tracing::warn!("Failed to query local models from Ollama: {}.", err);
        }
    }

    Ok(())
}

/// Ollama lists local models with an explicit tag, and an untagged name means `latest`.
/// Without this, `-m llama3.2` would re-pull (and need the registry) on every launch.
fn is_same_model(listed: &str, requested: &str) -> bool {
    listed == requested
        || (!has_tag(requested) && listed.strip_suffix(":latest") == Some(requested))
}

fn has_tag(model: &str) -> bool {
    // A registry host may carry a port, so only the final path segment can hold the tag.
    model
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains(':'))
}

fn min_responses_version() -> Version {
    Version::new(0, 13, 4)
}

fn supports_responses(version: &Version) -> bool {
    *version == Version::new(0, 0, 0) || *version >= min_responses_version()
}

/// Ensure the running Ollama server is new enough to support the Responses API.
///
/// Returns `Ok(())` when the version endpoint is missing or unparsable.
async fn ensure_responses_supported(client: &OllamaClient) -> std::io::Result<()> {
    let Some(version) = client.fetch_version().await? else {
        return Ok(());
    };

    if supports_responses(&version) {
        return Ok(());
    }

    let min = min_responses_version();
    Err(std::io::Error::other(format!(
        "Ollama {version} is too old. Codex requires Ollama {min} or newer."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_responses_for_dev_zero() {
        assert!(supports_responses(&Version::new(0, 0, 0)));
    }

    #[test]
    fn does_not_support_responses_before_cutoff() {
        assert!(!supports_responses(&Version::new(0, 13, 3)));
    }

    #[test]
    fn supports_responses_at_or_after_cutoff() {
        assert!(supports_responses(&Version::new(0, 13, 4)));
        assert!(supports_responses(&Version::new(0, 14, 0)));
    }

    #[test]
    fn untagged_requests_match_the_latest_local_listing() {
        assert!(is_same_model("llama3.2:latest", "llama3.2"));
        assert!(is_same_model("gpt-oss:20b", "gpt-oss:20b"));
        assert!(!is_same_model("gpt-oss:20b", "gpt-oss"));
        assert!(!is_same_model("llama3.2:latest", "llama3.2:1b"));
        assert!(is_same_model(
            "localhost:5000/team/model:latest",
            "localhost:5000/team/model"
        ));
        assert!(!is_same_model(
            "localhost:5000/team/model:v2",
            "localhost:5000/team/model"
        ));
    }
}
