use super::*;

use crate::session::tests::make_session_and_context;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
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

fn tool_history_receipt(call_id: &str) -> String {
    let artifact_sha256 = "a".repeat(64);
    let receipt_id = format!(
        "thr1-{}",
        &format!(
            "{:x}",
            Sha256::digest(
                format!("{call_id}:{artifact_sha256}:read_tool_output:read:123").as_bytes()
            )
        )[..16]
    );
    serde_json::json!({
        "version": 1,
        "receipt_id": receipt_id,
        "call_id": call_id,
        "tool_identity": "read_tool_output",
        "semantic_class": "read",
        "digest": "bounded evidence",
        "artifact": {
            "artifact_id": "019fd974-843a-7601-8624-dc36cd5cc3cd",
            "byte_start": 0,
            "byte_end": 123,
            "sha256": artifact_sha256,
            "complete": true
        },
        "original": {"bytes": 123, "approximate_tokens": 50},
        "retrieval": {
            "tool": "read_tool_output",
            "instruction": "recover narrowly"
        }
    })
    .to_string()
}

fn tool_search_group(call_id: &str) -> Vec<ResponseItem> {
    vec![
        ResponseItem::ToolSearchCall {
            id: None,
            call_id: Some(call_id.to_string()),
            status: Some("completed".to_string()),
            execution: "client".to_string(),
            arguments: serde_json::json!({
                "query": "calendar",
                "namespace": "apps"
            }),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ToolSearchOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            status: "completed".to_string(),
            execution: "client".to_string(),
            tools: vec![
                serde_json::json!({"namespace": "apps", "name": "calendar.search"}),
                serde_json::json!({"namespace": "apps", "name": "calendar.create"}),
            ],
            omitted_result_count: Some(0),
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

#[test]
fn remote_compaction_keeps_tool_outputs_with_recovery_references() {
    let artifact_reference = tool_history_receipt("call-1");
    let items = vec![
        function_call("call", "call-1"),
        function_call_output("output", "call-1", &artifact_reference),
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
            &tool_history_receipt("call-recoverable"),
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
                &tool_history_receipt("call-recoverable"),
            ),
            compaction
        ]
    );
}

#[test]
fn over_truncation_remote_compaction_keeps_exact_artifact_recovery_sidecar() {
    let compaction = ResponseItem::Compaction {
        id: None,
        encrypted_content: "opaque-state".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let payload = serde_json::json!({
        "version": 1,
        "kind": "tool_history_artifact_pins",
        "instruction": "Use read_tool_output with the retained artifact_id.",
        "artifacts": [{
            "artifact_id": "019fd974-843a-7601-8624-dc36cd5cc3cd",
            "sha256": "a".repeat(64),
            "bytes": 123
        }]
    })
    .to_string();

    let retained = append_remote_compaction_artifact_pins(vec![compaction], Some(payload));
    let ResponseItem::Message { content, .. } = &retained[1] else {
        panic!("expected deterministic artifact recovery sidecar");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected text sidecar");
    };

    assert!(text.contains("tool_history_artifact_pins"));
    assert!(text.contains("019fd974-843a-7601-8624-dc36cd5cc3cd"));
    assert!(text.contains("read_tool_output"));
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

#[test]
fn remote_compaction_preserves_search_query_and_ordered_result_identities() {
    let retained = bounded_remote_compacted_history(tool_search_group("search-1"));
    let ResponseItem::ToolSearchOutput { tools, .. } = &retained[1] else {
        panic!("expected retained search output");
    };
    let receipt = parse_remote_tool_search_receipt(&tools[0]).expect("typed search receipt");

    assert_eq!(receipt.arguments["query"], "calendar");
    assert_eq!(receipt.result_count, 2);
    assert_eq!(
        receipt.ordered_tool_identities,
        vec!["apps.calendar.search", "apps.calendar.create"]
    );
    assert!(receipt.complete);
}

#[test]
fn remote_search_receipt_bounds_arguments_and_rejects_semantic_tampering() {
    let mut items = tool_search_group("search-1");
    let ResponseItem::ToolSearchCall { arguments, .. } = &mut items[0] else {
        panic!("expected search call");
    };
    *arguments = serde_json::json!({
        "query": "q".repeat(20_000),
        "namespace": "n".repeat(20_000),
        "limit": ["large".repeat(20_000)],
        "cursor": "c".repeat(20_000),
    });
    let retained = bounded_remote_compacted_history(items);
    let ResponseItem::ToolSearchOutput { tools, .. } = &retained[1] else {
        panic!("expected retained search output");
    };
    let receipt = parse_remote_tool_search_receipt(&tools[0]).expect("typed search receipt");
    assert!(
        approx_token_count(&serde_json::to_string(&receipt).expect("serialize receipt"))
            <= TOOL_SEARCH_RECEIPT_MAX_TOKENS
    );
    assert!(receipt.arguments.get("query_sha256").is_some());
    assert!(receipt.arguments.get("namespace_sha256").is_some());
    assert!(receipt.arguments.get("limit_sha256").is_some());
    assert!(receipt.arguments.get("cursor_sha256").is_some());
    assert!(remote_tool_search_receipt_is_valid(
        &receipt,
        "search-1",
        "completed",
        "client"
    ));

    let mut tampered = receipt;
    tampered.status = "failed".to_string();
    assert!(!remote_tool_search_receipt_is_valid(
        &tampered, "search-1", "failed", "client"
    ));
}

#[tokio::test]
async fn trim_function_call_history_scans_past_non_output_boundaries() {
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
    let search = tool_search_group("search-1");
    let recent_unrecoverable =
        custom_tool_call_output("recent-output-id", "recent-call-id", &"b".repeat(8_192));
    turn_context.model_info.context_window = Some(REMOTE_COMPACTION_TRANSPORT_RESERVE_TOKENS + 1);
    turn_context.model_info.effective_context_window_percent = 100;

    let mut history = ContextManager::new();
    history.replace(vec![
        prefix,
        search[0].clone(),
        search[1].clone(),
        rewrite_boundary.clone(),
        recent_unrecoverable.clone(),
    ]);
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

    assert_eq!(rewritten_outputs, 1);
    let ResponseItem::ToolSearchOutput { tools, .. } = &history.raw_items()[2] else {
        panic!("expected rewritten search output");
    };
    let receipt = parse_remote_tool_search_receipt(&tools[0]).expect("typed search receipt");
    assert!(!receipt.complete);
    assert_eq!(history.raw_items()[3], rewrite_boundary);
    assert_eq!(history.raw_items()[4], recent_unrecoverable);
    assert!(estimated_tokens_after < estimated_tokens_before);
    assert_eq!(
        estimated_deleted_tokens,
        estimated_tokens_before - estimated_tokens_after
    );
}

#[test]
fn trimmed_nonempty_tool_search_becomes_a_structured_nonempty_receipt() {
    let items = tool_search_group("search-1");

    let rewritten = rewritten_output_for_context_window(&items, 1).expect("search receipt");
    let ResponseItem::ToolSearchOutput { tools, .. } = rewritten else {
        panic!("expected search output");
    };
    let receipt = parse_remote_tool_search_receipt(&tools[0]).expect("typed search receipt");

    assert_eq!(receipt.arguments["query"], "calendar");
    assert_eq!(receipt.result_count, 2);
    assert_eq!(
        receipt.ordered_tool_identities,
        vec!["apps.calendar.search", "apps.calendar.create"]
    );
    assert!(!receipt.complete);
}

#[tokio::test]
async fn prepared_prompt_size_does_not_rewrite_an_output_already_absent_from_projection() {
    let (_session, mut turn_context) = make_session_and_context().await;
    let base_instructions = BaseInstructions {
        text: String::new(),
    };
    let prefix = message(
        "prefix-id",
        "user",
        ContentItem::InputText {
            text: "prepared prefix".to_string(),
        },
    );
    let output = function_call_output("output-id", "call-id", &"x".repeat(20_000));
    let mut history = ContextManager::new();
    history.replace(vec![prefix.clone(), output.clone()]);
    let prepared_tokens = estimate_item_token_count(&prefix);
    turn_context.model_info.context_window =
        Some(prepared_tokens.saturating_add(REMOTE_COMPACTION_TRANSPORT_RESERVE_TOKENS));
    turn_context.model_info.effective_context_window_percent = 100;

    let (rewritten, _) = trim_function_call_history_to_fit_context_window_for_prompt(
        &mut history,
        &turn_context,
        &base_instructions,
        Some(&[prefix]),
    );

    assert_eq!(rewritten, 0);
    assert_eq!(
        history.raw_items(),
        &[
            message(
                "prefix-id",
                "user",
                ContentItem::InputText {
                    text: "prepared prefix".to_string(),
                },
            ),
            output
        ]
    );
}
