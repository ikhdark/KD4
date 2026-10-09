use super::*;

use crate::session::tests::make_session_and_context;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use pretty_assertions::assert_eq;

// Opt-in microbenchmark: excludes session setup and fixture construction. This
// measures local preparation only, not provider latency or end-to-end turns.
#[test]
#[ignore]
fn benchmark_compaction_search_receipt() {
    let mut items = tool_search_group("benchmark");
    if let ResponseItem::ToolSearchOutput { tools, .. } = &mut items[1] {
        *tools = (0..256).map(|index| serde_json::json!({
            "namespace": "apps", "name": format!("tool_{index:04}"),
            "description": "schema documentation ".repeat(64),
        })).collect();
    }
    let mut samples = Vec::new();
    for _ in 0..9 {
        let started = std::time::Instant::now();
        let (_, _, output) = remote_tool_search_receipt_group(
            std::hint::black_box(&items), &[0, 1], true,
        ).expect("bounded receipt");
        samples.push(started.elapsed().as_micros());
        std::hint::black_box(output);
    }
    samples.sort_unstable();
    eprintln!("compaction_search_receipt_us={samples:?} median={}", samples[4]);
    if let Some(directory) = std::env::var_os("COMPACTION_BENCHMARK_DIR") {
        std::fs::write(std::path::PathBuf::from(directory).join("search-receipt.txt"),
            format!("microseconds={samples:?}\nmedian={}\n", samples[4])).unwrap();
    }
}

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

#[test]
fn receipt_index_preserves_first_counterparts_and_rejects_incompatible_orphans() {
    let items = vec![
        function_call("first", "same"),
        function_call("duplicate", "same"),
        custom_tool_call_output("wrong-kind", "same", "receipt"),
        function_call_output("output", "same", "receipt"),
        function_call("orphan", "missing"),
    ];
    let index = tool_receipt_index(&items);
    assert_eq!(
        complete_tool_receipt_indices(&items, &index, 0),
        Some(vec![0, 3])
    );
    assert_eq!(
        complete_tool_receipt_indices(&items, &index, 1),
        Some(vec![1, 3])
    );
    assert_eq!(
        complete_tool_receipt_indices(&items, &index, 3),
        Some(vec![0, 3])
    );
    assert_eq!(complete_tool_receipt_indices(&items, &index, 2), None);
    assert_eq!(complete_tool_receipt_indices(&items, &index, 4), None);
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
fn receipt_prefix_search_matches_linear_reference_and_survives_recompaction() {
    for count in [0, 1, 9, 10, 40, 100, 256] {
        let mut items = tool_search_group("prefix-test");
        let identities = (0..count)
            .map(|index| format!("apps.tool_{index}_引用_\\\""))
            .collect::<Vec<_>>();
        let ResponseItem::ToolSearchOutput { tools, .. } = &mut items[1] else {
            unreachable!()
        };
        *tools = identities.iter().map(|name| serde_json::json!({"name": name})).collect();
        let expected_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(tools).unwrap()));
        let (_, call, output) = remote_tool_search_receipt_group(&items, &[0, 1], true).unwrap();
        let ResponseItem::ToolSearchOutput { tools, .. } = &output else { unreachable!() };
        let actual = parse_remote_tool_search_receipt(&tools[0]).unwrap();
        assert_eq!(actual.result_count, count);
        assert_eq!(actual.result_set_sha256, expected_hash);
        let expected = (0..=count).rev().find_map(|retained| {
            let mut candidate = actual.clone();
            candidate.ordered_tool_identities = identities[..retained].to_vec();
            candidate.omitted_identity_count = count - retained;
            candidate.receipt_id = remote_tool_search_receipt_id(
                &candidate.call_id, &candidate.status, &candidate.execution,
                &candidate.arguments, &candidate.result_set_sha256, candidate.result_count,
                candidate.omitted_result_count, candidate.complete,
                candidate.omitted_identity_count, &candidate.ordered_tool_identities,
            );
            (approx_token_count(&serde_json::to_string(&candidate).unwrap())
                <= TOOL_SEARCH_RECEIPT_MAX_TOKENS).then_some(candidate)
        }).unwrap();
        assert_eq!(actual, expected);
        let again = vec![call, output];
        let (_, _, repeated) = remote_tool_search_receipt_group(&again, &[0, 1], true).unwrap();
        assert_eq!(repeated, again[1]);
    }
}

#[tokio::test]
async fn compaction_fitting_never_expands_small_search_outputs() {
    let (_, mut turn) = make_session_and_context().await;
    turn.model_info.context_window = Some(REMOTE_COMPACTION_TRANSPORT_RESERVE_TOKENS + 1);
    turn.model_info.effective_context_window_percent = 100;
    let items = tool_search_group("small");
    let mut history = ContextManager::new();
    history.replace(items.clone());
    let result = trim_function_call_history_to_fit_context_window_for_prompt(
        &mut history, &turn, &BaseInstructions { text: String::new() },
        Some(&items), 0,
    );
    assert_eq!(result, (0, 0));
    assert_eq!(history.raw_items(), items);
}

#[test]
fn remote_compaction_keeps_tool_outputs_with_recovery_references() {
    let artifact_reference = tool_history_receipt("call-1");
    let items = vec![
        function_call("call", "call-1"),
        function_call_output("output", "call-1", &artifact_reference),
    ];

    assert_eq!(bounded_remote_compacted_history(items.clone(), response_item_has_valid_tool_history_receipt), items);
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

    let retained = bounded_remote_compacted_history(items, response_item_has_valid_tool_history_receipt);

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

    let retained = append_remote_compaction_artifact_pins(vec![compaction.clone()], Some(payload.clone()));
    assert_eq!(retained.len(), 2);
    assert_eq!(retained[0], compaction);
    assert!(crate::stable_context::is_trusted_stable_context_item(&retained[1]));
    assert!(!crate::compact::task_compaction_items(&retained).iter().any(|item|
        matches!(item, ResponseItem::Message { content, .. } if content.iter().any(|part|
            matches!(part, ContentItem::InputText { text } if text.contains("tool_history_artifact_pins"))))));
    let ResponseItem::Message { content, .. } = &retained[1] else {
        panic!("expected deterministic artifact recovery sidecar");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected text sidecar");
    };

    assert_eq!(text, &payload);
}

#[test]
fn remote_compaction_drops_nonrecoverable_tool_receipts() {
    let items = vec![
        function_call("plain-call", "call-plain"),
        function_call_output("plain", "call-plain", "successful consumed output"),
    ];

    assert!(bounded_remote_compacted_history(items, |_| false).is_empty());
}

#[test]
fn remote_compaction_drops_orphan_tool_receipts() {
    let items = vec![function_call_output(
        "orphan",
        "call-orphan",
        &tool_history_receipt("call-orphan"),
    )];

    assert!(bounded_remote_compacted_history(items, response_item_has_valid_tool_history_receipt).is_empty());
}

#[test]
fn remote_compaction_preserves_search_query_and_ordered_result_identities() {
    let retained = bounded_remote_compacted_history(tool_search_group("search-1"), |_| false);
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
    let retained = bounded_remote_compacted_history(items, |_| false);
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
    let mut search = tool_search_group("search-1");
    if let ResponseItem::ToolSearchOutput { tools, .. } = &mut search[1] {
        for tool in tools {
            tool["description"] = serde_json::json!("search documentation ".repeat(256));
        }
    }
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

    let rewritten = rewritten_output_for_context_window(&items, &tool_receipt_index(&items), 1)
        .expect("search receipt");
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
async fn request_schema_overhead_can_require_compaction_output_rewriting() {
    let (_, mut turn) = make_session_and_context().await;
    let mut items = tool_search_group("schema-pressure");
    let ResponseItem::ToolSearchOutput { tools, .. } = &mut items[1] else {
        unreachable!()
    };
    tools[0]["description"] = serde_json::Value::String("schema detail ".repeat(2_000));
    items.push(ResponseItem::CompactionTrigger {});
    let base = BaseInstructions {
        text: String::new(),
    };
    let item_tokens = items.iter().map(estimate_item_token_count).sum::<i64>();
    turn.model_info.context_window = Some(item_tokens + REMOTE_COMPACTION_TRANSPORT_RESERVE_TOKENS);
    turn.model_info.effective_context_window_percent = 100;
    let mut history = ContextManager::new();
    history.replace(items.clone());
    assert_eq!(
        trim_function_call_history_to_fit_context_window_for_prompt(
            &mut history,
            &turn,
            &base,
            Some(&items),
            0
        )
        .0,
        0
    );
    let (rewritten, savings) = trim_function_call_history_to_fit_context_window_for_prompt(
        &mut history,
        &turn,
        &base,
        Some(&items),
        2_000,
    );
    assert_eq!(rewritten, 1);
    assert!(savings > 2_000);
    assert!(matches!(
        history.raw_items().last(),
        Some(ResponseItem::CompactionTrigger {})
    ));
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
    let mut search = tool_search_group("absent-search");
    let ResponseItem::ToolSearchOutput { tools, .. } = &mut search[1] else {
        unreachable!()
    };
    tools[0]["description"] = serde_json::json!("large schema ".repeat(2_000));
    let mut items = vec![prefix.clone()];
    items.extend(search);
    let mut history = ContextManager::new();
    history.replace(items.clone());
    let prepared_tokens = estimate_item_token_count(&prefix);
    turn_context.model_info.context_window =
        Some(prepared_tokens.saturating_add(REMOTE_COMPACTION_TRANSPORT_RESERVE_TOKENS));
    turn_context.model_info.effective_context_window_percent = 100;

    // Ensure the omitted output is actually rewriteable and the projected
    // request remains over budget, so neither early return can mask a bug.
    let mut included = history.clone();
    assert_eq!(
        trim_function_call_history_to_fit_context_window_for_prompt(
            &mut included, &turn_context, &base_instructions, Some(&items), 1_000,
        ).0,
        1
    );
    let (rewritten, savings) = trim_function_call_history_to_fit_context_window_for_prompt(
        &mut history,
        &turn_context,
        &base_instructions,
        Some(&[prefix]),
        1_000,
    );

    assert_eq!(rewritten, 0);
    assert_eq!(savings, 0);
    assert_eq!(history.raw_items(), items);
}
