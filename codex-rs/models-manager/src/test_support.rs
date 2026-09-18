//! Test-only helpers exposed for dependent crate tests.
//!
//! Production code should not depend on this module.

use crate::ModelsManagerConfig;
use crate::bundled_models_response;
use crate::manager::construct_model_info_from_candidates;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;

/// Get model identifier without consulting remote state or cache.
#[expect(
    clippy::expect_used,
    reason = "Test fixtures must fail immediately if the bundled model catalog is invalid"
)]
pub fn get_model_offline_for_tests(model: Option<&str>) -> String {
    if let Some(model) = model {
        return model.to_string();
    }
    let mut response = bundled_models_response().expect("bundled test model catalog must load");
    response.models.sort_by_key(|model| model.priority);
    let presets: Vec<ModelPreset> = response.models.into_iter().map(Into::into).collect();
    presets
        .iter()
        .find(|preset| preset.show_in_picker)
        .or_else(|| presets.first())
        .map(|preset| preset.model.clone())
        .expect("bundled test model catalog must contain a default model")
}

/// Build `ModelInfo` without consulting remote state or cache.
#[expect(
    clippy::expect_used,
    reason = "Test fixtures must fail immediately if the bundled model catalog is invalid"
)]
pub fn construct_model_info_offline_for_tests(
    model: &str,
    config: &ModelsManagerConfig,
) -> ModelInfo {
    let candidates: &[ModelInfo] = if let Some(model_catalog) = config.model_catalog.as_ref() {
        &model_catalog.models
    } else {
        &crate::bundled_models()
            .expect("bundled test model catalog must load")
            .models
    };
    construct_model_info_from_candidates(model, candidates, config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ModelsResponse;
    use codex_protocol::openai_models::TruncationPolicyConfig;

    #[test]
    fn offline_helper_uses_bundled_metadata_and_honors_explicit_empty_catalogs() {
        let known =
            construct_model_info_offline_for_tests("gpt-5.4", &ModelsManagerConfig::default());
        assert!(!known.used_fallback_model_metadata);
        assert_eq!(
            known.truncation_policy,
            TruncationPolicyConfig::tokens(10_000)
        );
        let empty = construct_model_info_offline_for_tests(
            "gpt-5.4",
            &ModelsManagerConfig {
                model_catalog: Some(ModelsResponse { models: Vec::new() }),
                ..Default::default()
            },
        );
        assert!(empty.used_fallback_model_metadata);
        assert_eq!(
            empty.truncation_policy,
            TruncationPolicyConfig::bytes(10_000)
        );
    }
}
