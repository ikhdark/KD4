use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_5_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID;
use codex_models_manager::bundled_models;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;

// AWS model cards verified 2026-10-26 for GPT-5.5 and GPT-6 Astra, both endpoints.
// The backend ceiling is distinct from config's operating window and input reserve.
const BEDROCK_DOCUMENTED_CONTEXT_WINDOW: i64 = 1_050_000;
const GPT_5_5_OPENAI_MODEL_ID: &str = "gpt-5.5";

#[expect(
    clippy::expect_used,
    reason = "The embedded model catalog is validated by catalog tests and cannot change at runtime"
)]
pub(crate) fn static_model_catalog() -> ModelsResponse {
    let bundled = bundled_models().expect("bundled models.json should parse");
    let mut catalog = ModelsResponse {
        models: vec![
            bedrock_model(
                bundled_openai_model(bundled, "gpt-6-astra"),
                AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID,
                "GPT-6-Astra",
                /*priority*/ 0,
            ),
            bedrock_model(
                bundled_openai_model(bundled, "gpt-6-sol"),
                AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID,
                "GPT-6 Sol",
                /*priority*/ 1,
            ),
            bedrock_model(
                bundled_openai_model(bundled, "gpt-6-luna"),
                AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID,
                "GPT-6 Luna",
                /*priority*/ 2,
            ),
            bedrock_model(
                bundled_openai_model(bundled, "gpt-5.6-sol"),
                AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID,
                "GPT-5.6 Sol",
                /*priority*/ 3,
            ),
            bedrock_model(
                bundled_openai_model(bundled, "gpt-5.6-terra"),
                AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID,
                "GPT-5.6 Terra",
                /*priority*/ 4,
            ),
            bedrock_model(
                bundled_openai_model(bundled, "gpt-5.6-luna"),
                AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID,
                "GPT-5.6 Luna",
                /*priority*/ 5,
            ),
            bedrock_model(
                bundled_openai_model(bundled, GPT_5_5_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_5_5_MODEL_ID,
                "GPT-5.5",
                /*priority*/ 6,
            ),
        ],
    };
    for model in &mut catalog.models {
        // Constructors already normalized these selected owned models.
        clear_service_tiers(model);
    }
    catalog
}

pub(crate) fn with_default_only_service_tier(mut catalog: ModelsResponse) -> ModelsResponse {
    for model in &mut catalog.models {
        normalize_bedrock_model(model);
        clear_service_tiers(model);
    }
    catalog
}

fn clear_service_tiers(model: &mut ModelInfo) {
    // Amazon Bedrock currently only supports the implicit "default" tier for GPT models.
    model.additional_speed_tiers.clear();
    model.service_tiers.clear();
    model.default_service_tier = None;
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
    if let Some(ceiling) = documented_context_ceiling(&model.slug) {
        // The bundled OpenAI operating window is not Bedrock's default.
        model.context_window = Some(ceiling);
    }
    model
}

fn normalize_bedrock_model(model: &mut ModelInfo) {
    if let Some(ceiling) = documented_context_ceiling(&model.slug) {
        model.max_context_window = Some(ceiling);
        // Preserve an explicit catalog's smaller operating window.
        model.context_window = model.context_window.map(|window| window.min(ceiling));
    }
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

fn documented_context_ceiling(slug: &str) -> Option<i64> {
    let slug = slug
        .strip_prefix("global.")
        .or_else(|| slug.strip_prefix("us."))
        .unwrap_or(slug);
    matches!(
        slug,
        AMAZON_BEDROCK_GPT_5_5_MODEL_ID | AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID
    )
    .then_some(BEDROCK_DOCUMENTED_CONTEXT_WINDOW)
}

fn bundled_openai_model(catalog: &ModelsResponse, slug: &str) -> ModelInfo {
    catalog
        .models
        .iter()
        .find(|model| model.slug == slug)
        .unwrap_or_else(|| panic!("bundled models.json should include {slug}"))
        .clone()
}

