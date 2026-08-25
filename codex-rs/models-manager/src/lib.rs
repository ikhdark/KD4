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

/// Convert the client version string to a whole version string (e.g. "1.2.3-alpha.4" -> "1.2.3").
pub fn client_version_to_whole() -> String {
    format!(
        "{}.{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_catalog_reuses_one_parsed_normalized_instance() {
        let first = super::bundled_models().expect("bundled models should parse");
        let second = super::bundled_models().expect("bundled models should remain available");

        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn model_catalog_does_not_export_tui_migration_prompt_keys() {
        let library_source = include_str!("lib.rs");
        assert!(!library_source.contains(&["pub mod ", "model_presets;"].concat()));
    }

    #[test]
    fn models_manager_config_is_owned_by_the_crate_root() {
        let library_source = include_str!("lib.rs");

        assert!(library_source.contains(&["pub struct ", "ModelsManagerConfig"].concat()));
        assert!(!library_source.contains(&["mod ", "config;"].concat()));
    }
}
