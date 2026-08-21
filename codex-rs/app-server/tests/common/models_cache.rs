use codex_models_manager::bundled_models_response;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use std::path::Path;

pub const TEST_MODEL_CATALOG_FILENAME: &str = "models_test_catalog.json";

/// Install a static model catalog in the Codex home directory.
///
/// The historical function name is retained for call-site compatibility. Test
/// catalogs must use `model_catalog_json`: runtime model caches are scoped to
/// the complete provider and authentication identity and cannot be fabricated
/// before the app-server resolves that identity.
/// Uses the complete bundled catalog so hidden models and model-message metadata remain intact.
pub fn write_models_cache(codex_home: &Path) -> std::io::Result<()> {
    let catalog = bundled_models_response().map_err(std::io::Error::other)?;
    write_models_cache_with_models(codex_home, catalog.models)
}

/// Install a static model catalog with specific models.
/// Useful when tests need specific models to be available.
pub fn write_models_cache_with_models(
    codex_home: &Path,
    models: Vec<ModelInfo>,
) -> std::io::Result<()> {
    let catalog_path = codex_home.join(TEST_MODEL_CATALOG_FILENAME);
    let catalog = ModelsResponse { models };
    std::fs::write(&catalog_path, serde_json::to_string_pretty(&catalog)?)?;

    let config_path = codex_home.join("config.toml");
    let config = if config_path.try_exists()? {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };
    let config = config
        .strip_prefix("model_catalog_json = ")
        .and_then(|rest| rest.split_once('\n').map(|(_, tail)| tail))
        .unwrap_or(&config);
    let catalog_path = serde_json::to_string(&catalog_path.to_string_lossy())?;
    let contents = format!("model_catalog_json = {catalog_path}\n{config}");
    std::fs::write(config_path, contents)
}
