use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_core::CodexThread;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem as CoreDynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse as CoreDynamicToolResponse;
use codex_protocol::protocol::Op;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

use crate::image_url::image_url_error;
use crate::outgoing_message::ClientRequestResult;
use crate::server_request_error::is_turn_transition_server_request_error;

pub(crate) async fn on_call_response(
    turn_id: String,
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
            turn_id,
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
            let mut reason = "unsupported image representation";
            response.content_items.retain(|item| {
                let error = match item {
                    DynamicToolCallOutputContentItem::InputImage { image_url } => {
                        image_url_error(image_url)
                    }
                    _ => None,
                };
                if let Some(error) = error {
                    reason = error;
                }
                let invalid = error.is_some();
                omitted += usize::from(invalid);
                !invalid
            });
            if omitted == 0 {
                return (response, None);
            }
            let message = format!(
                "Partial dynamic tool output: omitted {omitted} unsupported image item(s). {reason}. The success flag describes the client's reported execution outcome; output delivery is incomplete."
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

