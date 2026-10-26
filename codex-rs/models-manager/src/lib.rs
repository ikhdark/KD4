pub(crate) mod cache;
pub mod collaboration_mode_presets;
pub mod manager;
pub mod model_info;
mod prompt_resolver;

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

/// Borrow the validated, prompt-policy-applied immutable bundled catalog.
/// Callers needing ownership can use [`bundled_models_response`].
pub fn bundled_models() -> Result<&'static ModelsResponse, serde_json::Error> {
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
    for model in &response.models {
        model
            .validate_capabilities()
            .map_err(<serde_json::Error as serde::de::Error>::custom)?;
    }
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

