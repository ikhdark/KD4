use codex_protocol::protocol::MultiAgentVersion;
use pretty_assertions::assert_eq;

use super::static_runtime_model_catalog;

#[test]
fn runtime_catalog_preserves_cross_region_order_and_supported_capabilities() {
    let catalog = static_runtime_model_catalog();

    assert_eq!(
        catalog
            .models
            .iter()
            .map(|model| (
                model.slug.as_str(),
                model.display_name.as_str(),
                model.priority,
            ))
            .collect::<Vec<_>>(),
        vec![
            ("global.openai.gpt-6-astra", "GPT-6-Astra (Global)", 0),
            ("global.openai.gpt-6-sol", "GPT-6 Sol (Global)", 1),
            ("global.openai.gpt-6-luna", "GPT-6 Luna (Global)", 2),
            ("global.openai.gpt-5.6-sol", "GPT-5.6 Sol (Global)", 3),
            ("global.openai.gpt-5.6-terra", "GPT-5.6 Terra (Global)", 4),
            ("global.openai.gpt-5.6-luna", "GPT-5.6 Luna (Global)", 5),
            ("us.openai.gpt-6-astra", "GPT-6-Astra (US cross-region)", 6),
            ("us.openai.gpt-6-sol", "GPT-6 Sol (US cross-region)", 7),
            ("us.openai.gpt-6-luna", "GPT-6 Luna (US cross-region)", 8),
            ("us.openai.gpt-5.6-sol", "GPT-5.6 Sol (US cross-region)", 9),
            (
                "us.openai.gpt-5.6-terra",
                "GPT-5.6 Terra (US cross-region)",
                10
            ),
            (
                "us.openai.gpt-5.6-luna",
                "GPT-5.6 Luna (US cross-region)",
                11
            ),
        ]
    );
    for model in catalog.models {
        assert!(!model.supports_search_tool, "{}", model.slug);
        assert_eq!(model.multi_agent_version, Some(MultiAgentVersion::V1));
        assert!(!model.use_responses_lite);
        assert_eq!(model.tool_mode, None);
    }
}
