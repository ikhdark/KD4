use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_5_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID;
use codex_models_manager::bundled_models_response;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;

const GPT_5_BEDROCK_CONTEXT_WINDOW: i64 = 272_000;
const GPT_5_5_OPENAI_MODEL_ID: &str = "gpt-5.5";

#[expect(
    clippy::expect_used,
    reason = "The embedded model catalog is validated by catalog tests and cannot change at runtime"
)]
pub(crate) fn static_model_catalog() -> ModelsResponse {
    let bundled = bundled_models_response().expect("bundled models.json should parse");
    with_default_only_service_tier(ModelsResponse {
        models: vec![
            bedrock_model(
                bundled_openai_model(&bundled, "gpt-6-astra"),
                AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID,
                "GPT-6-Astra",
                /*priority*/ 0,
            ),
            bedrock_model(
                bundled_openai_model(&bundled, "gpt-6-sol"),
                AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID,
                "GPT-6 Sol",
                /*priority*/ 1,
            ),
            bedrock_model(
                bundled_openai_model(&bundled, "gpt-6-luna"),
                AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID,
                "GPT-6 Luna",
                /*priority*/ 2,
            ),
            bedrock_model(
                bundled_openai_model(&bundled, "gpt-5.6-sol"),
                AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID,
                "GPT-5.6 Sol",
                /*priority*/ 3,
            ),
            bedrock_model(
                bundled_openai_model(&bundled, "gpt-5.6-terra"),
                AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID,
                "GPT-5.6 Terra",
                /*priority*/ 4,
            ),
            bedrock_model(
                bundled_openai_model(&bundled, "gpt-5.6-luna"),
                AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID,
                "GPT-5.6 Luna",
                /*priority*/ 5,
            ),
            gpt_5_bedrock_model(
                bundled_openai_model(&bundled, GPT_5_5_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_5_5_MODEL_ID,
                "GPT-5.5",
                /*priority*/ 6,
            ),
        ],
    })
}

pub(crate) fn with_default_only_service_tier(mut catalog: ModelsResponse) -> ModelsResponse {
    for model in &mut catalog.models {
        normalize_bedrock_model(model);
        // Amazon Bedrock currently only supports the implicit "default" tier for GPT models.
        model.additional_speed_tiers.clear();
        model.service_tiers.clear();
        model.default_service_tier = None;
    }
    catalog
}

fn bedrock_model(
    mut model: ModelInfo,
    bedrock_slug: &str,
    display_name: &str,
    priority: i32,
) -> ModelInfo {
    model.availability_nux = None;
    model.upgrade = None;
    model.slug = bedrock_slug.to_string();
    model.display_name = display_name.to_string();
    model.priority = priority;
    model.visibility = ModelVisibility::List;
    normalize_bedrock_model(&mut model);
    model
}

fn normalize_bedrock_model(model: &mut ModelInfo) {
    // Bedrock uses Responses and direct tools, without Codex-only Ultra effort.
    model.use_responses_lite = false;
    model.tool_mode = None;
    // Bedrock cannot accept the response items used by multi-agent V2.
    model.multi_agent_version = Some(codex_protocol::protocol::MultiAgentVersion::V1);
    model.web_search_tool_type = codex_protocol::openai_models::WebSearchToolType::Text;
    model
        .supported_reasoning_levels
        .retain(|level| level.effort != ReasoningEffort::Ultra);
    if let Some(default) = &model.default_reasoning_level
        && !model
            .supported_reasoning_levels
            .iter()
            .any(|level| &level.effort == default)
    {
        model.default_reasoning_level = model
            .supported_reasoning_levels
            .first()
            .map(|level| level.effort.clone());
    }
}

fn gpt_5_bedrock_model(
    model: ModelInfo,
    bedrock_slug: &str,
    display_name: &str,
    priority: i32,
) -> ModelInfo {
    let mut model = bedrock_model(model, bedrock_slug, display_name, priority);
    model.context_window = Some(GPT_5_BEDROCK_CONTEXT_WINDOW);
    model.max_context_window = Some(GPT_5_BEDROCK_CONTEXT_WINDOW);
    model
}

fn bundled_openai_model(catalog: &ModelsResponse, slug: &str) -> ModelInfo {
    catalog
        .models
        .iter()
        .find(|model| model.slug == slug)
        .unwrap_or_else(|| panic!("bundled models.json should include {slug}"))
        .clone()
}

#[cfg(test)]
mod tests {
    use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn configured_catalog_enforces_compatibility_without_replacing_identity() {
        let mut model = bundled_openai_model(&bundled_models_response().unwrap(), "gpt-6-astra");
        model.slug = "custom-model".into();
        model.default_reasoning_level = Some(ReasoningEffort::Ultra);
        model.use_responses_lite = true;
        let instructions = model.base_instructions.clone();
        let window = model.context_window;
        let catalog = with_default_only_service_tier(ModelsResponse {
            models: vec![model],
        });
        let model = &catalog.models[0];
        assert_eq!(model.slug, "custom-model");
        assert_eq!(model.base_instructions, instructions);
        assert_eq!(model.context_window, window);
        assert!(!model.use_responses_lite);
        assert_eq!(model.tool_mode, None);
        assert_eq!(
            model.multi_agent_version,
            Some(codex_protocol::protocol::MultiAgentVersion::V1)
        );
        assert!(
            !model
                .supported_reasoning_levels
                .iter()
                .any(|level| level.effort == ReasoningEffort::Ultra)
        );
        assert!(
            model
                .supported_reasoning_levels
                .iter()
                .any(|level| Some(&level.effort) == model.default_reasoning_level.as_ref())
        );
    }

    #[test]
    fn adapted_templates_handle_existing_max_and_filtered_default() {
        let bundled = bundled_models_response().unwrap();
        let template = bedrock_model(
            bundled_openai_model(&bundled, "gpt-6-sol"),
            "test",
            "test",
            0,
        );
        let adapted = bedrock_model(template, "test", "test", 0);
        assert_eq!(
            adapted
                .supported_reasoning_levels
                .iter()
                .filter(|level| level.effort == ReasoningEffort::Max)
                .count(),
            1
        );
        let mut template = bundled_openai_model(&bundled, "gpt-6-astra");
        template.default_reasoning_level = Some(ReasoningEffort::Ultra);
        let adapted = bedrock_model(template, "test", "test", 0);
        assert!(
            !adapted
                .supported_reasoning_levels
                .iter()
                .any(|level| level.effort == ReasoningEffort::Ultra)
        );
        assert!(
            adapted
                .supported_reasoning_levels
                .iter()
                .any(|level| Some(&level.effort) == adapted.default_reasoning_level.as_ref())
        );
    }

    #[test]
    fn catalog_preserves_model_order_and_enforces_bedrock_limits() {
        let catalog = static_model_catalog();

        assert_eq!(
            catalog
                .models
                .iter()
                .map(|model| model.slug.as_str())
                .collect::<Vec<_>>(),
            vec![
                AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID,
                AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID,
                AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID,
                AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID,
                AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID,
                AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID,
                AMAZON_BEDROCK_GPT_5_5_MODEL_ID,
            ]
        );
        for model in catalog.models {
            let expected = match model.slug.as_str() {
                AMAZON_BEDROCK_GPT_5_5_MODEL_ID => (
                    Some(GPT_5_BEDROCK_CONTEXT_WINDOW),
                    Some(GPT_5_BEDROCK_CONTEXT_WINDOW),
                ),
                _ => (Some(272_000), Some(872_000)),
            };
            assert_eq!(
                (model.context_window, model.max_context_window),
                expected,
                "{}",
                model.slug
            );
            assert_eq!((&model.availability_nux, &model.upgrade), (&None, &None));
            assert_eq!(model.additional_speed_tiers, Vec::<String>::new());
            assert_eq!(model.service_tiers, Vec::new());
            assert_eq!(model.default_service_tier, None);
            assert_eq!(
                model.service_tier_for_request(Some("priority".to_string())),
                None
            );
            assert_eq!(
                model
                    .service_tier_for_request(Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string())),
                None
            );
        }
    }

    #[test]
    fn bedrock_models_preserve_source_metadata_with_supported_capabilities() {
        let catalog = static_model_catalog();
        let bundled = bundled_models_response().unwrap();

        for (slug, display_name, priority) in [
            (AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID, "GPT-6-Astra", 0),
            (AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID, "GPT-6 Sol", 1),
            (AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID, "GPT-6 Luna", 2),
            (AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID, "GPT-5.6 Sol", 3),
            (AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID, "GPT-5.6 Terra", 4),
            (AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID, "GPT-5.6 Luna", 5),
        ] {
            let mut expected =
                bundled_openai_model(&bundled, slug.strip_prefix("openai.").unwrap());
            expected.slug = slug.to_string();
            expected.display_name = display_name.to_string();
            expected.priority = priority;
            expected.visibility = ModelVisibility::List;
            expected.availability_nux = None;
            expected.upgrade = None;
            expected.use_responses_lite = false;
            expected.tool_mode = None;
            expected.multi_agent_version = Some(codex_protocol::protocol::MultiAgentVersion::V1);
            expected.web_search_tool_type = codex_protocol::openai_models::WebSearchToolType::Text;
            expected.additional_speed_tiers.clear();
            expected.service_tiers.clear();
            expected.default_service_tier = None;
            expected
                .supported_reasoning_levels
                .retain(|level| level.effort != ReasoningEffort::Ultra);
            let actual = catalog
                .models
                .iter()
                .find(|model| model.slug == slug)
                .unwrap()
                .clone();
            assert_eq!(
                actual
                    .supported_reasoning_levels
                    .iter()
                    .filter(|level| level.effort == ReasoningEffort::Max)
                    .count(),
                1
            );
            assert_eq!(actual, expected);
        }
    }

}
