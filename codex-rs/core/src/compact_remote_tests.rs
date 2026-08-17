use super::*;

use crate::session::tests::make_session_and_context;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;

fn message(id: &str, role: &str, content: ContentItem) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.to_string())),
        role: role.to_string(),
        content: vec![content],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call_output(id: &str, call_id: &str, output: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: Some(ResponseItemId::from_server(id.to_string())),
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(output.to_string()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(id: &str, call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: Some(ResponseItemId::from_server(id.to_string())),
        name: "read_tool_output".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn custom_tool_call_output(id: &str, call_id: &str, output: &str) -> ResponseItem {
    ResponseItem::CustomToolCallOutput {
        id: Some(ResponseItemId::from_server(id.to_string())),
        call_id: call_id.to_string(),
        name: Some("custom-tool".to_string()),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(output.to_string()),
            success: Some(false),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn remote_compaction_keeps_tool_outputs_with_recovery_references() {
    let artifact_reference = "[command output reduced; full retained output is available as artifact 019fd974-843a-7601-8624-dc36cd5cc3cd.\nUse read_tool_output with that id.]";
    let items = vec![
        function_call("call", "call-1"),
        function_call_output("output", "call-1", artifact_reference),
    ];

    assert_eq!(bounded_remote_compacted_history(items.clone()), items);
}

#[test]
fn remote_compaction_evicts_raw_messages_and_bounds_tool_receipts() {
    let compaction = ResponseItem::Compaction {
        id: None,
        encrypted_content: "opaque-state".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let items = vec![
        message(
            "raw-user",
            "user",
            ContentItem::InputText {
                text: "consumed ".repeat(20_000),
            },
        ),
        function_call("oversized-call", "call-oversized"),
        function_call_output("oversized", "call-oversized", &"x".repeat(20_000)),
        function_call("plain-call", "call-plain"),
        function_call_output("plain", "call-plain", "artifact 123"),
        function_call("recoverable-call", "call-recoverable"),
        function_call_output(
            "recoverable",
            "call-recoverable",
            "Raw output artifact: 019fd974-843a-7601-8624-dc36cd5cc3cd (123 bytes retained)",
        ),
        compaction.clone(),
    ];

    let retained = bounded_remote_compacted_history(items);

    assert_eq!(
        retained,
        vec![
            function_call("recoverable-call", "call-recoverable"),
            function_call_output(
                "recoverable",
                "call-recoverable",
                "Raw output artifact: 019fd974-843a-7601-8624-dc36cd5cc3cd (123 bytes retained)",
            ),
            compaction
        ]
    );
}

#[test]
fn remote_compaction_drops_nonrecoverable_tool_receipts() {
    let items = vec![
        function_call("plain-call", "call-plain"),
        function_call_output("plain", "call-plain", "successful consumed output"),
    ];

    assert!(bounded_remote_compacted_history(items).is_empty());
}

#[test]
fn remote_compaction_drops_orphan_tool_receipts() {
    let items = vec![function_call_output(
        "orphan",
        "call-orphan",
        "artifact 123",
    )];

    assert!(bounded_remote_compacted_history(items).is_empty());
}

#[tokio::test]
async fn trim_function_call_history_rewrites_contiguous_trailing_outputs_in_one_pass() {
    let (_session, mut turn_context) = make_session_and_context().await;
    let base_instructions = BaseInstructions {
        text: String::new(),
    };
    let prefix = message(
        "prefix-id",
        "user",
        ContentItem::InputText {
            text: "unchanged prefix".to_string(),
        },
    );
    let rewrite_boundary = message(
        "boundary-id",
        "assistant",
        ContentItem::OutputText {
            text: "non-output rewrite boundary".to_string(),
        },
    );
    let first_output = function_call_output("first-output-id", "first-call-id", &"a".repeat(8_192));
    let second_output =
        custom_tool_call_output("second-output-id", "second-call-id", &"b".repeat(8_192));
    let expected_first_output = function_call_output(
        "first-output-id",
        "first-call-id",
        CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE,
    );
    let expected_second_output = custom_tool_call_output(
        "second-output-id",
        "second-call-id",
        CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE,
    );
    let expected_items = vec![
        prefix.clone(),
        rewrite_boundary.clone(),
        expected_first_output,
        expected_second_output,
    ];

    let mut expected_history = ContextManager::new();
    expected_history.replace(expected_items.clone());
    let expected_tokens_after = expected_history
        .estimate_token_count_with_base_instructions(&base_instructions)
        .expect("expected token estimate");
    turn_context.model_info.context_window = Some(expected_tokens_after.saturating_sub(1));
    turn_context.model_info.effective_context_window_percent = 100;

    let mut history = ContextManager::new();
    history.replace(vec![prefix, rewrite_boundary, first_output, second_output]);
    let estimated_tokens_before = history
        .estimate_token_count_with_base_instructions(&base_instructions)
        .expect("token estimate before rewrite");

    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window(
            &mut history,
            &turn_context,
            &base_instructions,
        );
    let estimated_tokens_after = history
        .estimate_token_count_with_base_instructions(&base_instructions)
        .expect("token estimate after rewrite");

    assert_eq!(rewritten_outputs, 2);
    assert_eq!(history.raw_items(), expected_items);
    assert!(
        estimated_tokens_after
            > turn_context
                .model_context_window()
                .expect("configured context window"),
        "rewriting must stop at the distinct non-output boundary"
    );
    assert_eq!(
        estimated_deleted_tokens,
        estimated_tokens_before - estimated_tokens_after
    );
}
