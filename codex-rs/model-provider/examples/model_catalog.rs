//! Offline provider-catalog diagnostic; no credentials or provider requests.
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::ModelsManagerConfig;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let mut endpoints = Vec::new();
    for (id, info) in [
        (
            "amazon-bedrock",
            ModelProviderInfo::create_amazon_bedrock_provider(None),
        ),
        (
            "amazon-bedrock-runtime",
            ModelProviderInfo::create_amazon_bedrock_runtime_provider(None),
        ),
    ] {
        let provider = create_model_provider(info, None);
        let manager = provider.models_manager(id, std::env::temp_dir(), None);
        let mut models = runtime.block_on(manager.get_remote_models());
        let mut observations = Vec::new();
        let requested = models
            .iter()
            .map(|m| m.slug.clone())
            .chain(std::env::args().skip(1))
            .collect::<Vec<_>>();
        for slug in requested {
            let model =
                runtime.block_on(manager.get_model_info(&slug, &ModelsManagerConfig::default()));
            let mut metadata = serde_json::to_value(&model)?;
            metadata
                .as_object_mut()
                .ok_or("model is not an object")?
                .remove("base_instructions");
            observations.push(json!({
                "requested": slug,
                "fallback": model.used_fallback_model_metadata,
                "local_prompt": model.base_instructions == codex_protocol::models::default_base_instructions(),
                "ultrafast_request": model.service_tier_for_request(Some("ultrafast".to_string())),
                "default_request": model.service_tier_for_request(Some("default".to_string())),
                "priority_request": model.service_tier_for_request(Some("priority".to_string())),
                "metadata": metadata,
            }));
        }
        for model in &mut models {
            model.visibility = ModelVisibility::List;
        }
        let mut without_tiers = models.clone();
        for model in &mut without_tiers {
            model.service_tiers.clear();
            model.additional_speed_tiers.clear();
            model.default_service_tier = None;
        }
        let restricted = provider.models_manager(
            id,
            std::env::temp_dir(),
            Some(ModelsResponse {
                models: without_tiers,
            }),
        );
        let restricted_models = runtime.block_on(restricted.get_remote_models());
        let mut authored_tiers = models.clone();
        for model in &mut authored_tiers {
            for tier in &mut model.service_tiers {
                tier.name = "Operator ultrafast".to_string();
                tier.description = "Operator-owned tier description".to_string();
            }
        }
        let authored = provider.models_manager(
            id,
            std::env::temp_dir(),
            Some(ModelsResponse {
                models: authored_tiers,
            }),
        );
        let authored_models = runtime.block_on(authored.get_remote_models());
        let custom =
            provider.models_manager(id, std::env::temp_dir(), Some(ModelsResponse { models }));
        let custom_models = runtime.block_on(custom.get_remote_models());
        endpoints.push(json!({
            "provider": id,
            "observations": observations,
            "explicit_catalog_presets": custom.build_available_models(custom_models),
            "explicit_catalog_without_tiers": restricted.build_available_models(restricted_models),
            "explicit_catalog_authored_tiers": authored.build_available_models(authored_models),
        }));
    }
    println!("{}", serde_json::to_string_pretty(&endpoints)?);
    Ok(())
}
