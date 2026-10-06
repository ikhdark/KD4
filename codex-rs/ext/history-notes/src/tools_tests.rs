use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolOutput;
use codex_tools::ToolPayload;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::HistoryNotesToolOutput;

#[test]
fn log_preview_preserves_json_truncation_and_retained_result() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    for result in [
        json!({"text": "short \"quoted\" text\nnext line"}),
        json!({"encrypted_output": "x".repeat(65_536)}),
        json!({"text": "é🦀\n".repeat(4_096), "metadata": {"source": "item-1"}}),
    ] {
        let expected = codex_tools::JsonToolOutput::new(result.clone()).log_preview();
        let output = HistoryNotesToolOutput::new(result.clone()).expect("valid output");
        let response = output.to_response_item("call-1", &payload);

        assert_eq!(output.log_preview(), expected);
        assert_eq!(output.log_preview(), expected);
        assert_eq!(
            output.post_tool_use_response("call-1", &payload),
            Some(result)
        );
        assert_eq!(output.to_response_item("call-1", &payload), response);
    }
}

#[test]
fn preserves_encrypted_history_output() {
    let result = HistoryNotesToolOutput::new(json!({"encrypted_output": "enc_payload"}))
        .expect("valid output")
        .to_response_item(
            "call-1",
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );

    let ResponseInputItem::FunctionCallOutput { output, .. } = result else {
        panic!("expected function-call output");
    };
    assert_eq!(
        output.content_items(),
        Some(
            [FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "enc_payload".to_string(),
            }]
            .as_slice()
        )
    );
}

#[test]
fn preserves_images_as_separate_output_items_without_logging_bytes() {
    let output = HistoryNotesToolOutput::new(json!({
        "encrypted_output": "enc_payload",
        "images": [
            {"data": "cG5n", "mime_type": "image/png", "detail": "original"},
            {"data": "anBlZw==", "mime_type": "image/jpeg", "detail": "low"},
            {"data": "Z2lm", "mime_type": "image/gif"},
            {"data": "d2VicA==", "mime_type": "image/webp", "detail": null}
        ]
    }))
    .expect("valid image output");
    assert_eq!(
        output.log_preview(),
        json!({"encrypted_output": "enc_payload"}).to_string()
    );
    assert_eq!(
        output.post_tool_use_response(
            "call-1",
            &ToolPayload::Function {
                arguments: "{}".to_string()
            }
        ),
        Some(json!({"encrypted_output": "enc_payload"}))
    );
    assert_eq!(
        output.to_response_item(
            "call-1",
            &ToolPayload::Function {
                arguments: "{}".to_string()
            }
        ),
        ResponseInputItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: "enc_payload".to_string()
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,cG5n".to_string(),
                    detail: Some(ImageDetail::Original)
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/jpeg;base64,anBlZw==".to_string(),
                    detail: Some(ImageDetail::Low)
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/gif;base64,Z2lm".to_string(),
                    detail: None
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/webp;base64,d2VicA==".to_string(),
                    detail: None
                },
            ])
        }
    );
}

#[test]
fn accepts_empty_attachments_and_legacy_plaintext_results() {
    for (result, expected) in [
        (
            json!({"encrypted_output": "enc_payload", "images": []}),
            FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: "enc_payload".to_string(),
                },
            ]),
        ),
        (
            json!({"text": "legacy result"}),
            FunctionCallOutputPayload::from_text(json!({"text": "legacy result"}).to_string()),
        ),
    ] {
        let output = HistoryNotesToolOutput::new(result).expect("valid output");
        assert_eq!(
            output.to_response_item(
                "call-1",
                &ToolPayload::Function {
                    arguments: "{}".to_string()
                }
            ),
            ResponseInputItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: expected
            }
        );
    }
}

#[test]
fn rejects_malformed_attachments_instead_of_silently_dropping_them() {
    for images in [
        json!(null),
        json!({}),
        json!([null]),
        json!([{"mime_type": "image/png"}]),
        json!([{"data": "private-image-bytes"}]),
        json!([{"data": "private-image-bytes", "mime_type": "image/png", "detail": "invalid"}]),
    ] {
        let result = HistoryNotesToolOutput::new(
            json!({"encrypted_output": "enc_payload", "images": images}),
        );
        let Err(codex_extension_api::FunctionCallError::RespondToModel(message)) = result else {
            panic!("expected a model-facing image error");
        };
        assert_eq!(message, "History backend returned invalid image content.");
    }
}
