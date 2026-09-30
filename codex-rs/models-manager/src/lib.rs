pub(crate) mod cache;
pub mod collaboration_mode_presets;
pub mod manager;
pub mod model_info;
#[cfg(test)]
mod prompt_contract_tests;
mod prompt_resolver;
pub mod test_support;

pub use codex_protocol::auth::AuthMode;
use codex_protocol::openai_models::ModelsResponse;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct ModelsManagerConfig {
    pub model_context_window: Option<i64>,
    pub model_auto_compact_token_limit: Option<i64>,
    pub tool_output_token_limit: Option<usize>,
    pub base_instructions: Option<String>,
    pub personality_enabled: bool,
    pub model_catalog: Option<ModelsResponse>,
}

static BUNDLED_MODELS: OnceLock<ModelsResponse> = OnceLock::new();
static BUNDLED_MODELS_INIT: Mutex<()> = Mutex::new(());

pub(crate) fn bundled_models() -> Result<&'static ModelsResponse, serde_json::Error> {
    if let Some(response) = BUNDLED_MODELS.get() {
        return Ok(response);
    }

    let _init_guard = BUNDLED_MODELS_INIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(response) = BUNDLED_MODELS.get() {
        return Ok(response);
    }

    let mut response: ModelsResponse = serde_json::from_str(include_str!("../models.json"))?;
    prompt_resolver::apply_prompt_policy(&mut response.models);
    Ok(BUNDLED_MODELS.get_or_init(|| response))
}

/// Load the bundled model catalog shipped with `codex-models-manager`.
pub fn bundled_models_response() -> Result<ModelsResponse, serde_json::Error> {
    Ok(bundled_models()?.clone())
}

// The workspace uses 0.0.0 for local source builds, not as a protocol capability
// version. The /models endpoint filters individual models by client_version:
// 0.0.0 omits Sol 6/Luna 6, and 0.158.0 omits Sol 6.1. Inference rejects those
// models for a 0.0.0 client identity the same way. Use the baseline verified
// against rust-v0.159.1 without changing executable versions.
const SOURCE_BUILD_MODELS_CLIENT_VERSION: &str = "0.159.1";

/// Whole client version used consistently for model discovery and cache eligibility.
/// Source builds use the supported catalog baseline instead of the 0.0.0 placeholder.
pub fn client_version_to_whole() -> String {
    model_catalog_client_version(env!("CARGO_PKG_VERSION"))
}

/// Baseline to advertise in place of the 0.0.0 source-build placeholder.
/// Release versions, including prereleases, are advertised unchanged.
pub fn source_build_client_version(version: &str) -> Option<&'static str> {
    (whole_version(version) == "0.0.0").then_some(SOURCE_BUILD_MODELS_CLIENT_VERSION)
}

fn whole_version(version: &str) -> &str {
    version
        .split_once(['-', '+'])
        .map_or(version, |(whole, _)| whole)
}

fn model_catalog_client_version(package_version: &str) -> String {
    let whole = whole_version(package_version);
    source_build_client_version(whole)
        .unwrap_or(whole)
        .to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn model_catalog_client_version_handles_source_and_release_builds() {
        for (package_version, expected) in [
            ("0.0.0", "0.159.1"),
            ("0.0.0-dev+local", "0.159.1"),
            ("0.158.0", "0.158.0"),
            ("0.159.0-alpha.4", "0.159.0"),
            ("0.159.1", "0.159.1"),
            ("1.2.3+local", "1.2.3"),
            ("1.2.3-alpha.4+local", "1.2.3"),
            ("0.99.0", "0.99.0"),
        ] {
            assert_eq!(
                super::model_catalog_client_version(package_version),
                expected
            );
        }
        assert_ne!(super::client_version_to_whole(), "0.0.0");
        assert_eq!(
            super::source_build_client_version("0.0.0-dev+local"),
            Some("0.159.1")
        );
        assert_eq!(super::source_build_client_version("0.159.0-alpha.4"), None);
    }

    #[test]
    fn bundled_luna_6_preserves_runtime_metadata_and_local_prompt() {
        use codex_protocol::openai_models::ModelPreset;
        use codex_protocol::openai_models::ReasoningEffort;
        use codex_protocol::openai_models::ToolMode;
        use codex_protocol::protocol::MultiAgentVersion;

        let response = super::bundled_models_response().expect("bundled catalog");
        let luna = response
            .models
            .into_iter()
            .find(|model| model.slug == "gpt-6-luna")
            .expect("Luna 6");
        assert_eq!(luna.tool_mode, Some(ToolMode::CodeModeOnly));
        assert_eq!(luna.multi_agent_version, Some(MultiAgentVersion::V2));
        assert!(luna.use_responses_lite && luna.supports_parallel_tool_calls);
        assert_eq!(
            (luna.context_window, luna.max_context_window),
            (Some(272_000), Some(872_000))
        );
        assert_eq!(
            luna.get_model_instructions(None),
            codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.trim()
        );
        let preset: ModelPreset = luna.into();
        assert!(preset.show_in_picker);
        assert_eq!(preset.default_reasoning_effort, ReasoningEffort::Medium);
        assert_eq!(
            preset
                .supported_reasoning_efforts
                .into_iter()
                .map(|level| level.effort)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max
            ]
        );
    }

    #[test]
    fn bundled_catalog_reuses_one_parsed_normalized_instance() {
        let first = super::bundled_models().expect("bundled models should parse");
        let second = super::bundled_models().expect("bundled models should remain available");

        assert!(std::ptr::eq(first, second));
    }
}
