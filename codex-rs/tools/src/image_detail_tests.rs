use super::*;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::openai_models::ModelInfo;
use pretty_assertions::assert_eq;
use serde_json::json;

fn model_info() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "test-model",
        "display_name": "Test Model",
        "description": null,
        "supported_reasoning_levels": [],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": "base",
        "model_messages": null,
        "supports_reasoning_summaries": false,
        "default_reasoning_summary": "auto",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {
            "mode": "bytes",
            "limit": 10000
        },
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": true,
        "context_window": null,
        "auto_compact_token_limit": null,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": false
    }))
    .expect("deserialize test model")
}

#[test]
fn can_request_original_image_detail_follows_model_capability() {
    for supports_original in [false, true] {
        let mut model = model_info();
        model.supports_image_detail_original = supports_original;
        assert_eq!(can_request_original_image_detail(&model), supports_original);
    }
}

#[test]
fn sanitize_original_falls_back_to_high_without_support() {
    let mut items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "header".to_string(),
        },
        FunctionCallOutputContentItem::InputImage {
            image_url: "data:image/png;base64,AAA".to_string(),
            detail: Some(ImageDetail::Original),
        },
        FunctionCallOutputContentItem::InputImage {
            image_url: "data:image/png;base64,BBB".to_string(),
            detail: Some(ImageDetail::Low),
        },
    ];

    let original = items.clone();
    sanitize_original_image_detail(/*can_request_original_image_detail*/ true, &mut items);
    assert_eq!(items, original);

    sanitize_original_image_detail(/*can_request_original_image_detail*/ false, &mut items);

    assert_eq!(
        items,
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "header".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAA".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,BBB".to_string(),
                detail: Some(ImageDetail::Low),
            },
        ]
    );
}
