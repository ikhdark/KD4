//! Offline model-catalog diagnostic. Does not authenticate or call a provider.
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let catalog = codex_models_manager::bundled_models_response()?;
    let manager = StaticModelsManager::new(None, catalog.clone());
    let presets = manager.build_available_models(catalog.models);
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let mut resolutions = Vec::new();
    for slug in std::env::args().skip(1) {
        let model =
            runtime.block_on(manager.get_model_info(&slug, &ModelsManagerConfig::default()));
        let local_prompt =
            model.base_instructions == codex_protocol::models::default_base_instructions();
        let fallback = model.used_fallback_model_metadata;
        let overridden = runtime.block_on(manager.get_model_info(
            &slug,
            &ModelsManagerConfig {
                model_context_window: Some(900_000),
                base_instructions: Some("diagnostic override".to_string()),
                ..Default::default()
            },
        ));
        let mut metadata = serde_json::to_value(model)?;
        metadata
            .as_object_mut()
            .ok_or("model metadata is not an object")?
            .remove("base_instructions");
        resolutions.push(json!({
            "requested": slug,
            "fallback": fallback,
            "local_prompt": local_prompt,
            "metadata": metadata,
            "overridden_context_window": overridden.context_window,
            "prompt_override_preserved": overridden.base_instructions == "diagnostic override",
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "client_version": codex_models_manager::client_version_to_whole(),
            "presets": presets,
            "resolutions": resolutions,
        }))?
    );
    Ok(())
}
