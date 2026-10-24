use pretty_assertions::assert_eq;
use serde_json::json;

use super::ExtensionItem;
use super::image_generation::ImageGenerationItem;
use super::web_search::WebSearchAction;
use super::web_search::WebSearchItem;

fn completed_image_generation_item() -> ExtensionItem {
    ExtensionItem::ImageGeneration(ImageGenerationItem {
        id: "image-1".to_string(),
        status: "completed".to_string(),
        revised_prompt: Some("A blue square".to_string()),
        result: Some("cG5n".to_string()),
        error: None,
        saved_path: None,
        provider_usage: None,
    })
}

#[test]
fn image_generation_item_preserves_stable_wire_shape() {
    let item = completed_image_generation_item();
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "image_gen.generation",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": "A blue square",
            "result": "cG5n",
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
}

#[test]
fn image_generation_item_preserves_absence_and_failure_details() {
    for result in [None, Some(serde_json::Value::Null)] {
        let mut value = json!({
            "kind": "image_gen.generation", "id": "image-failed", "status": "failed",
            "revisedPrompt": null, "error": {"message":"Generation failed"},
        });
        if let Some(result) = result {
            value["result"] = result;
        }
        let item: ExtensionItem = serde_json::from_value(value).expect("unavailable image");
        let ExtensionItem::ImageGeneration(image) = &item else {
            panic!("image")
        };
        assert!(image.result.is_none());
        assert!(image.saved_path.is_none());
        assert_eq!(
            image.error.as_ref().unwrap()["message"],
            "Generation failed"
        );
        let serialized = serde_json::to_value(item).unwrap();
        assert!(serialized["result"].is_null());
        assert_eq!(serialized["error"]["message"], "Generation failed");
    }
}

#[test]
fn web_search_item_preserves_stable_wire_shape() {
    let item = ExtensionItem::WebSearch(WebSearchItem {
        status: None,
        error: None,
        id: "search-1".to_string(),
        query: "docs".to_string(),
        action: Some(WebSearchAction::Search {
            query: Some("docs".to_string()),
            queries: None,
        }),
    });
    let value = serde_json::to_value(&item).expect("serialize extension item");

    assert_eq!(
        value,
        json!({
            "kind": "web.search",
            "id": "search-1",
            "query": "docs",
            "action": {
                "type": "search",
                "query": "docs",
                "queries": null,
            },
        })
    );
    assert_eq!(
        serde_json::from_value::<ExtensionItem>(value).expect("deserialize extension item"),
        item
    );
}

#[test]
fn invalid_extension_payloads_are_rejected_for_the_expected_reason() {
    for (value, expected) in [
        (
            json!({"kind": "image_gen.unknown", "id": "image-1"}),
            "unknown variant `image_gen.unknown`",
        ),
        (
            json!({"kind": "image_gen.generation", "id": "image-1"}),
            "missing field `status`",
        ),
    ] {
        let error = serde_json::from_value::<ExtensionItem>(value).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}
