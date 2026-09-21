use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_core::CodexThread;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem as CoreDynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse as CoreDynamicToolResponse;
use codex_protocol::protocol::Op;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

use crate::image_url::REMOTE_IMAGE_URL_ERROR;
use crate::image_url::is_remote_image_url;
use crate::outgoing_message::ClientRequestResult;
use crate::server_request_error::is_turn_transition_server_request_error;

pub(crate) async fn on_call_response(
    call_id: String,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
) {
    let response = receiver.await;
    let (response, _error) = match response {
        Ok(Ok(value)) => decode_response(value),
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return,
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            fallback_response(&format!(
                "dynamic tool client error (code {}); execution outcome unknown. Check the operation's state before retrying.",
                err.code
            ))
        }
        Err(err) => {
            error!("request failed: {err:?}");
            fallback_response(
                "dynamic tool reply lost; execution outcome unknown. Check the operation's state before retrying.",
            )
        }
    };

    let core_response = into_core_response(response);
    if let Err(err) = conversation
        .submit(Op::DynamicToolResponse {
            id: call_id.clone(),
            response: core_response,
        })
        .await
    {
        error!("failed to submit DynamicToolResponse: {err}");
    }
}

fn into_core_response(response: DynamicToolCallResponse) -> CoreDynamicToolResponse {
    let DynamicToolCallResponse {
        content_items,
        success,
    } = response;
    CoreDynamicToolResponse {
        content_items: content_items
            .into_iter()
            .map(CoreDynamicToolCallOutputContentItem::from)
            .collect(),
        success,
    }
}

fn decode_response(value: serde_json::Value) -> (DynamicToolCallResponse, Option<String>) {
    match serde_json::from_value::<DynamicToolCallResponse>(value) {
        Ok(mut response) => {
            let mut omitted = 0;
            response.content_items.retain(|item| {
                let invalid = matches!(item, DynamicToolCallOutputContentItem::InputImage { image_url } if is_remote_image_url(image_url));
                omitted += usize::from(invalid);
                !invalid
            });
            if omitted == 0 {
                return (response, None);
            }
            let message = format!(
                "Partial dynamic tool output: omitted {omitted} unsupported image item(s). {REMOTE_IMAGE_URL_ERROR}. The success flag describes the client's reported execution outcome; output delivery is incomplete."
            );
            response
                .content_items
                .push(DynamicToolCallOutputContentItem::InputText {
                    text: message.clone(),
                });
            (response, Some(message))
        }
        Err(err) => {
            error!("failed to deserialize DynamicToolCallResponse: {err}");
            // serde errors can embed arbitrary payload values. Expose location and
            // category without copying untrusted response contents into the error.
            fallback_response(&format!(
                "dynamic tool response schema invalid ({:?}, line {}, column {}); execution outcome unknown",
                err.classify(),
                err.line(),
                err.column()
            ))
        }
    }
}

fn fallback_response(message: &str) -> (DynamicToolCallResponse, Option<String>) {
    (
        DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: message.to_string(),
            }],
            success: false,
        },
        Some(message.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_response_conversion_moves_owned_content() {
        let text = "owned dynamic tool response".to_string();
        let text_ptr = text.as_ptr();
        let response = DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText { text }],
            success: true,
        };

        let core_response = into_core_response(response);
        assert_eq!(core_response.content_items.len(), 1);
        let CoreDynamicToolCallOutputContentItem::InputText { text } =
            &core_response.content_items[0]
        else {
            panic!("dynamic text should remain text after protocol conversion");
        };

        assert!(core_response.success);
        assert_eq!(text, "owned dynamic tool response");
        assert_eq!(
            text.as_ptr(),
            text_ptr,
            "the protocol conversion should move owned response strings",
        );
    }
    #[test]
    fn mixed_dynamic_output_preserves_text_and_reports_incomplete_delivery() {
        let (response, diagnostic) = decode_response(serde_json::json!({
            "success": true,
            "contentItems": [
                {"type":"inputText", "text":"operation receipt 123"},
                {"type":"inputImage", "imageUrl":"https://example.com/image.png"}
            ]
        }));
        assert!(response.success);
        assert_eq!(response.content_items.len(), 2);
        assert!(
            matches!(&response.content_items[0], DynamicToolCallOutputContentItem::InputText { text } if text == "operation receipt 123")
        );
        assert!(diagnostic.unwrap().contains("Partial dynamic tool output"));
    }

    #[test]
    fn malformed_dynamic_output_does_not_disclose_payload_values() {
        let (response, diagnostic) =
            decode_response(serde_json::json!({"success":"private-token", "contentItems":[]}));
        assert!(!response.success);
        let diagnostic = diagnostic.unwrap();
        assert!(diagnostic.contains("schema invalid"));
        assert!(!diagnostic.contains("private-token"));
    }
}
