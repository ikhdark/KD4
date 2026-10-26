use codex_app_server_protocol::McpToolCallResult;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use serde_json::Value as JsonValue;

// Temporary bandaid for remote clients: thread/resume can include large MCP and
// image-generation payloads. Keep this response-only so persisted rollout
// history, model resume history, and other APIs stay unchanged.
const REDACTED_PAYLOAD: &str = "[redacted]";
const CHATGPT_REMOTE_CLIENT_NAMES: &[&str] =
    &["codex_chatgpt_android_remote", "codex_chatgpt_ios_remote"];

pub(super) fn should_redact_thread_resume_payloads(client_name: Option<&str>) -> bool {
    client_name.is_some_and(|client_name| CHATGPT_REMOTE_CLIENT_NAMES.contains(&client_name))
}

pub(super) fn redact_thread_resume_payloads(turns: &mut [Turn]) {
    for turn in turns {
        turn.items.retain_mut(|item| match item {
            ThreadItem::McpToolCall {
                arguments,
                result,
                error,
                ..
            } => {
                *arguments = JsonValue::String(REDACTED_PAYLOAD.to_string());
                if result.is_some() {
                    *result = Some(Box::new(redacted_mcp_tool_call_result()));
                }
                if let Some(error) = error {
                    error.message = REDACTED_PAYLOAD.to_string();
                }
                true
            }
            ThreadItem::ImageGeneration(_) => false,
            _ => true,
        });
    }
}

fn redacted_mcp_tool_call_result() -> McpToolCallResult {
    McpToolCallResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": REDACTED_PAYLOAD,
        })],
        structured_content: None,
        meta: None,
    }
}

