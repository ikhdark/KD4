use std::path::Path;
use std::sync::Arc;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::PromptSlot;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolPayload;
use codex_tools::ToolCallSource;
use codex_tools::ToolOutput;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::PathExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;

use crate::extension::MemoriesExtension;
use crate::extension::MemoriesExtensionConfig;
use crate::local::LocalMemoriesBackend;

#[test]
fn tools_are_not_contributed_without_thread_config() {
    let extension = MemoriesExtension::default();

    assert!(
        extension
            .tools(
                &ExtensionData::new("session"),
                &ExtensionData::new("thread")
            )
            .is_empty()
    );
}

#[test]
fn tools_are_not_contributed_when_disabled() {
    let extension = MemoriesExtension::default();
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(MemoriesExtensionConfig {
        enabled: false,
        dedicated_tools: true,
        codex_home: test_path_buf("/tmp/codex-home").abs(),
    });

    assert!(
        extension
            .tools(&ExtensionData::new("session"), &thread_store)
            .is_empty()
    );
}

#[test]
fn tools_are_not_contributed_when_dedicated_tools_disabled() {
    let extension = MemoriesExtension::default();
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(MemoriesExtensionConfig {
        enabled: true,
        dedicated_tools: false,
        codex_home: test_path_buf("/tmp/codex-home").abs(),
    });

    assert!(
        extension
            .tools(&ExtensionData::new("session"), &thread_store)
            .is_empty()
    );
}

#[test]
fn tools_are_contributed_when_enabled_with_dedicated_tools() {
    let extension = MemoriesExtension::default();
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(MemoriesExtensionConfig {
        enabled: true,
        dedicated_tools: true,
        codex_home: test_path_buf("/tmp/codex-home").abs(),
    });

    let tool_names = extension
        .tools(&ExtensionData::new("session"), &thread_store)
        .into_iter()
        .map(|tool| tool.tool_name())
        .collect::<Vec<_>>();

    assert_eq!(
        tool_names,
        vec![
            memory_tool_name(crate::ADD_AD_HOC_NOTE_TOOL_NAME),
            memory_tool_name(crate::LIST_TOOL_NAME),
            memory_tool_name(crate::READ_TOOL_NAME),
            memory_tool_name(crate::SEARCH_TOOL_NAME),
        ]
    );
}

#[test]
fn install_registers_dedicated_tool_contributor() {
    let mut builder = ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    crate::install(&mut builder, /*metrics_client*/ None);
    let registry = builder.build();
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(MemoriesExtensionConfig {
        enabled: true,
        dedicated_tools: true,
        codex_home: test_path_buf("/tmp/codex-home").abs(),
    });

    let tools = registry
        .tool_contributors()
        .iter()
        .flat_map(|contributor| contributor.tools(&ExtensionData::new("session"), &thread_store))
        .collect::<Vec<_>>();
    let tool_names = tools
        .iter()
        .map(|tool| tool.tool_name())
        .collect::<Vec<_>>();

    assert_eq!(
        tool_names,
        vec![
            ToolName::namespaced("memories", "add_ad_hoc_note"),
            ToolName::namespaced("memories", "list"),
            ToolName::namespaced("memories", "read"),
            ToolName::namespaced("memories", "search"),
        ]
    );
    for (tool, expected_name) in tools
        .iter()
        .zip(["add_ad_hoc_note", "list", "read", "search"])
    {
        let spec = serde_json::to_value(tool.spec()).expect("serialize registered tool spec");
        assert_eq!(spec.pointer("/name"), Some(&json!("memories")));
        assert_eq!(spec.pointer("/tools/0/name"), Some(&json!(expected_name)));
    }
}

#[test]
fn ad_hoc_tool_definition_includes_filename_contract() {
    let tool = memory_tool(
        Path::new("/tmp/codex-home/memories"),
        crate::ADD_AD_HOC_NOTE_TOOL_NAME,
    );
    let spec = serde_json::to_value(tool.spec()).expect("serialize tool spec");

    let filename = spec
        .pointer("/tools/0/parameters/properties/filename")
        .expect("filename parameter should be in tool schema");
    assert_eq!(filename.pointer("/type"), Some(&json!("string")));
    assert!(
        filename
            .pointer("/description")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|description| description.contains("YYYY-MM-DDTHH-MM-SS-<slug>.md"))
    );
}

#[test]
fn add_ad_hoc_request_owns_the_tool_input_contract() {
    let request: crate::backend::AddAdHocMemoryNoteRequest = serde_json::from_value(json!({
        "filename": "2026-05-26T13-42-08-remember-review-style.md",
        "note": "Remember to keep PR review comments concise.",
    }))
    .expect("deserialize add-ad-hoc request");
    assert_eq!(
        request,
        crate::backend::AddAdHocMemoryNoteRequest {
            filename: "2026-05-26T13-42-08-remember-review-style.md".to_string(),
            note: "Remember to keep PR review comments concise.".to_string(),
        }
    );

    let schema = crate::schema::input_schema_for::<crate::backend::AddAdHocMemoryNoteRequest>();
    assert_eq!(
        schema.pointer("/properties/filename/type"),
        Some(&json!("string"))
    );
    assert!(
        serde_json::from_value::<crate::backend::AddAdHocMemoryNoteRequest>(json!({
            "filename": "2026-05-26T13-42-08-remember-review-style.md",
            "note": "Remember this.",
            "unexpected": true,
        }))
        .is_err()
    );
}

#[tokio::test]
async fn prompt_contribution_uses_memory_summary_when_enabled() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let memories_dir = tempdir.path().join("memories");
    tokio::fs::create_dir_all(&memories_dir)
        .await
        .expect("create memories dir");
    tokio::fs::write(
        memories_dir.join("memory_summary.md"),
        "Remember repository-specific implementation preferences.",
    )
    .await
    .expect("write memory summary");

    let extension = MemoriesExtension::default();
    let thread_store = ExtensionData::new("thread");
    thread_store.insert(MemoriesExtensionConfig {
        enabled: true,
        dedicated_tools: false,
        codex_home: tempdir.path().abs(),
    });

    let fragments = extension
        .contribute_thread_context(&ExtensionData::new("session"), &thread_store)
        .await;

    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].slot(), PromptSlot::DeveloperPolicy);
    assert_eq!(
        fragments[0].kind(),
        codex_extension_api::PromptFragmentKind::Memory
    );
    assert!(
        fragments[0]
            .text()
            .contains("Remember repository-specific implementation preferences.")
    );
}

#[tokio::test]
async fn add_ad_hoc_note_tool_creates_note_file() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let memory_root = tempdir.path().join("memories");
    let tool = memory_tool(&memory_root, crate::ADD_AD_HOC_NOTE_TOOL_NAME);
    let payload = ToolPayload::Function {
        arguments: json!({
            "filename": "2026-05-26T13-42-08-remember-review-style.md",
            "note": "Remember to keep PR review comments concise.",
        })
        .to_string(),
    };

    let output = tool
        .handle(ToolCall {
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: memory_tool_name(crate::ADD_AD_HOC_NOTE_TOOL_NAME),
            model: "gpt-test".to_string(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            source: ToolCallSource::Direct,
            conversation_history: codex_extension_api::ConversationHistory::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload: payload.clone(),
        })
        .await
        .expect("ad-hoc note should be written");

    assert_eq!(
        output.post_tool_use_response("call-1", &payload),
        Some(json!({}))
    );
    assert_eq!(
        tokio::fs::read_to_string(
            memory_root
                .join("extensions/ad_hoc/notes")
                .join("2026-05-26T13-42-08-remember-review-style.md")
        )
        .await
        .expect("read ad-hoc note"),
        "Remember to keep PR review comments concise."
    );
}

#[tokio::test]
async fn add_ad_hoc_note_tool_rejects_paths_as_filenames() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let memory_root = tempdir.path().join("memories");
    let tool = memory_tool(&memory_root, crate::ADD_AD_HOC_NOTE_TOOL_NAME);
    let payload = ToolPayload::Function {
        arguments: json!({
            "filename": "../2026-05-26T13-42-08-remember-review-style.md",
            "note": "Remember to keep PR review comments concise.",
        })
        .to_string(),
    };

    let result = tool
        .handle(ToolCall {
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: memory_tool_name(crate::ADD_AD_HOC_NOTE_TOOL_NAME),
            model: "gpt-test".to_string(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            source: ToolCallSource::Direct,
            conversation_history: codex_extension_api::ConversationHistory::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload,
        })
        .await;
    let err = match result {
        Ok(_) => panic!("path-like filename should be rejected"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("filename"));
    assert!(err.to_string().contains("YYYY-MM-DDTHH-MM-SS"));
}

#[tokio::test]
async fn read_tool_reads_memory_file() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let memory_root = tempdir.path().join("memories");
    tokio::fs::create_dir_all(&memory_root)
        .await
        .expect("create memories dir");
    tokio::fs::write(
        memory_root.join("MEMORY.md"),
        "first line\nsecond needle line\nthird line\n",
    )
    .await
    .expect("write memory");
    let tool = memory_tool(&memory_root, crate::READ_TOOL_NAME);
    let payload = ToolPayload::Function {
        arguments: json!({
            "path": "MEMORY.md",
            "line_offset": 2,
            "max_lines": 1
        })
        .to_string(),
    };

    let output = tool
        .handle(ToolCall {
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: memory_tool_name(crate::READ_TOOL_NAME),
            model: "gpt-test".to_string(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            source: ToolCallSource::Direct,
            conversation_history: codex_extension_api::ConversationHistory::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload: payload.clone(),
        })
        .await
        .expect("read should succeed");

    assert_eq!(
        output.post_tool_use_response("call-1", &payload),
        Some(json!({
            "path": "MEMORY.md",
            "content": "second needle line\n",
            "start_line_number": 2,
            "truncated": true
        }))
    );
}

#[tokio::test]
async fn search_tool_accepts_multiple_queries() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let memory_root = tempdir.path().join("memories");
    tokio::fs::create_dir_all(&memory_root)
        .await
        .expect("create memories dir");
    tokio::fs::write(
        memory_root.join("MEMORY.md"),
        "alpha only\nneedle only\nalpha needle\n",
    )
    .await
    .expect("write memory");
    let tool = memory_tool(&memory_root, crate::SEARCH_TOOL_NAME);
    let payload = ToolPayload::Function {
        arguments: json!({
            "queries": ["alpha", "needle"],
            "case_sensitive": false
        })
        .to_string(),
    };

    let output = tool
        .handle(ToolCall {
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: memory_tool_name(crate::SEARCH_TOOL_NAME),
            model: "gpt-test".to_string(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            source: ToolCallSource::Direct,
            conversation_history: codex_extension_api::ConversationHistory::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload: payload.clone(),
        })
        .await
        .expect("search should succeed");

    assert_eq!(
        output.post_tool_use_response("call-1", &payload),
        Some(json!({
            "queries": ["alpha", "needle"],
            "match_mode": {
                "type": "any"
            },
            "path": null,
            "matches": [
                {
                    "path": "MEMORY.md",
                    "match_line_number": 1,
                    "content_start_line_number": 1,
                    "content": "alpha only",
                    "content_truncated": false,
                    "matched_queries": ["alpha"]
                },
                {
                    "path": "MEMORY.md",
                    "match_line_number": 2,
                    "content_start_line_number": 2,
                    "content": "needle only",
                    "content_truncated": false,
                    "matched_queries": ["needle"]
                },
                {
                    "path": "MEMORY.md",
                    "match_line_number": 3,
                    "content_start_line_number": 3,
                    "content": "alpha needle",
                    "content_truncated": false,
                    "matched_queries": ["alpha", "needle"]
                }
            ],
            "next_cursor": null,
            "truncated": false
        }))
    );
}

#[tokio::test]
async fn search_tool_accepts_windowed_all_match_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let memory_root = tempdir.path().join("memories");
    tokio::fs::create_dir_all(&memory_root)
        .await
        .expect("create memories dir");
    tokio::fs::write(memory_root.join("MEMORY.md"), "alpha\nmiddle\nneedle\n")
        .await
        .expect("write memory");
    let tool = memory_tool(&memory_root, crate::SEARCH_TOOL_NAME);
    let payload = ToolPayload::Function {
        arguments: json!({
            "queries": ["alpha", "needle"],
            "match_mode": {
                "type": "all_within_lines",
                "line_count": 3
            }
        })
        .to_string(),
    };

    let output = tool
        .handle(ToolCall {
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: memory_tool_name(crate::SEARCH_TOOL_NAME),
            model: "gpt-test".to_string(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            source: ToolCallSource::Direct,
            conversation_history: codex_extension_api::ConversationHistory::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload: payload.clone(),
        })
        .await
        .expect("search should succeed");

    assert_eq!(
        output.post_tool_use_response("call-1", &payload),
        Some(json!({
            "queries": ["alpha", "needle"],
            "match_mode": {
                "type": "all_within_lines",
                "line_count": 3
            },
            "path": null,
            "matches": [
                {
                    "path": "MEMORY.md",
                    "match_line_number": 1,
                    "content_start_line_number": 1,
                    "content": "alpha\nmiddle\nneedle",
                    "content_truncated": false,
                    "matched_queries": ["alpha", "needle"]
                }
            ],
            "next_cursor": null,
            "truncated": false
        }))
    );
}

#[tokio::test]
async fn search_tool_rejects_legacy_single_query() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let memory_root = tempdir.path().join("memories");
    tokio::fs::create_dir_all(&memory_root)
        .await
        .expect("create memories dir");
    let tool = memory_tool(&memory_root, crate::SEARCH_TOOL_NAME);
    let payload = ToolPayload::Function {
        arguments: json!({
            "query": "needle",
        })
        .to_string(),
    };

    let result = tool
        .handle(ToolCall {
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool_name: memory_tool_name(crate::SEARCH_TOOL_NAME),
            model: "gpt-test".to_string(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            source: ToolCallSource::Direct,
            conversation_history: codex_extension_api::ConversationHistory::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload,
        })
        .await;
    let err = match result {
        Ok(_) => panic!("legacy query field should be rejected"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("unknown field"));
    assert!(err.to_string().contains("query"));
}

fn memory_tool(memory_root: &Path, tool_name: &str) -> Arc<dyn ToolExecutor<ToolCall>> {
    let expected_tool_name = memory_tool_name(tool_name);
    crate::tools::memory_tools(
        LocalMemoriesBackend::from_memory_root(memory_root),
        /*metrics_client*/ None,
    )
    .into_iter()
    .find(|tool| tool.tool_name() == expected_tool_name)
    .unwrap_or_else(|| panic!("{tool_name} tool should be registered"))
}

fn memory_tool_name(tool_name: &str) -> ToolName {
    ToolName::namespaced(crate::MEMORY_TOOLS_NAMESPACE, tool_name)
}

async fn invoke_memory_tool(
    root: &Path,
    name: &str,
    arguments: String,
) -> Result<serde_json::Value, codex_extension_api::FunctionCallError> {
    let tool = memory_tool(root, name);
    let payload = ToolPayload::Function { arguments };
    let output = tool
        .handle(ToolCall {
            turn_id: "turn-1".into(),
            call_id: "call-1".into(),
            tool_name: memory_tool_name(name),
            model: "gpt-test".into(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            source: ToolCallSource::Direct,
            conversation_history: Default::default(),
            turn_item_emitter: Arc::new(NoopTurnItemEmitter),
            cancellation_token: Default::default(),
            primary_environment_id: None,
            environments: Vec::new(),
            payload: payload.clone(),
        })
        .await?;
    Ok(output
        .post_tool_use_response("call-1", &payload)
        .expect("JSON output"))
}

#[tokio::test]
async fn search_pages_preserve_global_path_order_and_cursor_errors() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("a")).unwrap();
    for path in ["a/x.md", "a.md", "a0.md"] {
        std::fs::write(root.path().join(path), "needle\nneedle\n").unwrap();
    }
    let mut locations = Vec::new();
    let mut cursor = None;
    loop {
        let response = invoke_memory_tool(
            root.path(),
            crate::SEARCH_TOOL_NAME,
            json!({"queries": ["needle"], "max_results": 1, "cursor": cursor}).to_string(),
        )
        .await
        .unwrap();
        let matches = response["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        locations.push((
            matches[0]["path"].as_str().unwrap().to_string(),
            matches[0]["match_line_number"].as_u64().unwrap(),
        ));
        cursor = response["next_cursor"].as_str().map(str::to_owned);
        assert_eq!(response["truncated"], json!(cursor.is_some()));
        if cursor.is_none() {
            break;
        }
        assert!(locations.len() < 7, "cursor must advance");
    }
    assert_eq!(
        locations,
        vec![
            ("a.md".into(), 1),
            ("a.md".into(), 2),
            ("a/x.md".into(), 1),
            ("a/x.md".into(), 2),
            ("a0.md".into(), 1),
            ("a0.md".into(), 2)
        ]
    );
    let empty = invoke_memory_tool(
        root.path(),
        crate::SEARCH_TOOL_NAME,
        json!({"queries": ["needle"], "cursor": "6"}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(empty["matches"], json!([]));
    assert_eq!(empty["next_cursor"], json!(null));
    let err = invoke_memory_tool(
        root.path(),
        crate::SEARCH_TOOL_NAME,
        json!({"queries": ["needle"], "cursor": "7"}).to_string(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, codex_extension_api::FunctionCallError::RespondToModel(message) if message.contains("exceeds result count"))
    );
}

#[tokio::test]
async fn search_window_modes_return_only_minimal_windows() {
    let root = tempfile::tempdir().unwrap();
    let backend = LocalMemoriesBackend::from_memory_root(root.path());
    // Independently enumerate all valid ranges, then discard non-minimal ranges.
    for pattern in 0usize..64 {
        let lines = (0..3)
            .map(|idx| ["-", "a", "b", "ab"][(pattern >> (idx * 2)) & 3])
            .collect::<Vec<_>>();
        std::fs::write(root.path().join("memory.md"), lines.join("\n")).unwrap();
        for width in 1..=3 {
            let mut candidates = Vec::new();
            for start in 0..3 {
                for end in start..3 {
                    if end - start < width
                        && ["a", "b"]
                            .iter()
                            .all(|q| lines[start..=end].iter().any(|line| line.contains(q)))
                    {
                        candidates.push((start, end));
                    }
                }
            }
            let expected = candidates
                .iter()
                .copied()
                .filter(|&(start, end)| {
                    !candidates.iter().any(|&(other_start, other_end)| {
                        (start, end) != (other_start, other_end)
                            && start <= other_start
                            && end >= other_end
                    })
                })
                .map(|(start, end)| (start + 1, lines[start..=end].join("\n")))
                .collect::<Vec<_>>();
            let result = backend
                .search(crate::backend::SearchMemoriesRequest {
                    queries: vec!["a".into(), "b".into()],
                    match_mode: crate::backend::SearchMatchMode::AllWithinLines {
                        line_count: width,
                    },
                    path: None,
                    cursor: None,
                    context_lines: 0,
                    case_sensitive: true,
                    normalized: false,
                    max_results: 200,
                })
                .await
                .unwrap();
            assert_eq!(
                result
                    .matches
                    .iter()
                    .map(|found| (found.match_line_number, found.content.clone()))
                    .collect::<Vec<_>>(),
                expected,
                "pattern {pattern}, width {width}"
            );
            assert!(
                result
                    .matches
                    .iter()
                    .all(|found| found.matched_queries == ["a", "b"])
            );
        }
    }
}

#[tokio::test]
async fn search_excerpt_budget_is_separate_from_pagination() {
    let root = tempfile::tempdir().unwrap();
    let line = format!("needle{}", "\u{754c}".repeat(10_000));
    std::fs::write(root.path().join("memory.md"), vec![line; 8].join("\n")).unwrap();
    let first = invoke_memory_tool(
        root.path(),
        crate::SEARCH_TOOL_NAME,
        json!({"queries": ["needle"], "context_lines": usize::MAX}).to_string(),
    )
    .await
    .unwrap();
    let matches = first["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 5);
    assert!(
        matches
            .iter()
            .all(|found| found["content_truncated"] == true
                && found["content"].as_str().unwrap().starts_with("needle")
                && found["content"].as_str().unwrap().len() <= 16_000)
    );
    assert!(
        matches
            .iter()
            .map(|found| found["content"].as_str().unwrap().len())
            .sum::<usize>()
            <= 80_000
    );
    assert_eq!(first["next_cursor"], json!(matches.len().to_string()));
    let last = invoke_memory_tool(
        root.path(),
        crate::SEARCH_TOOL_NAME,
        json!({"queries": ["needle"], "cursor": "7", "context_lines": usize::MAX}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(last["matches"][0]["match_line_number"], json!(8));
    assert_eq!(last["matches"][0]["content_start_line_number"], json!(8));
    assert_eq!(last["matches"][0]["content_truncated"], json!(true));
    assert_eq!(last["truncated"], json!(false));
    assert_eq!(last["next_cursor"], json!(null));
}

#[tokio::test]
async fn read_ranges_preserve_newlines_empty_final_line_and_utf8_validation() {
    let root = tempfile::tempdir().unwrap();
    for (source, offset, limit, expected, truncated) in [
        ("", 1, 1, "", false),
        ("a\r\n\u{754c}\nlast", 2, 1, "\u{754c}\n", true),
        ("a\n", 2, 1, "", false),
        ("a\nb", 2, 3, "b", false),
    ] {
        std::fs::write(root.path().join("memory.md"), source).unwrap();
        let output = invoke_memory_tool(
            root.path(),
            crate::READ_TOOL_NAME,
            json!({"path":"memory.md", "line_offset":offset, "max_lines":limit}).to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            output,
            json!({"path":"memory.md", "start_line_number":offset, "content":expected, "truncated":truncated})
        );
    }
    let error = invoke_memory_tool(
        root.path(),
        crate::READ_TOOL_NAME,
        json!({"path":"memory.md", "line_offset":3}).to_string(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("exceeds file length"));
    std::fs::write(root.path().join("memory.md"), b"valid\n\xff").unwrap();
    let error = invoke_memory_tool(
        root.path(),
        crate::READ_TOOL_NAME,
        json!({"path":"memory.md", "max_lines":1}).to_string(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, codex_extension_api::FunctionCallError::Fatal(_)),
        "invalid UTF-8 outside the range must still fail"
    );
}

#[tokio::test]
async fn concurrent_note_creation_publishes_one_complete_note_and_cleans_staging() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("memories");
    let filename = "2026-09-13T13-42-08-concurrent.md";
    let first = "first note\n".repeat(20_000);
    let second = "second note\n".repeat(20_000);
    let (a, b) = tokio::join!(
        invoke_memory_tool(
            &root,
            crate::ADD_AD_HOC_NOTE_TOOL_NAME,
            json!({"filename":filename,"note":first}).to_string()
        ),
        invoke_memory_tool(
            &root,
            crate::ADD_AD_HOC_NOTE_TOOL_NAME,
            json!({"filename":filename,"note":second}).to_string()
        ),
    );
    let expected = match (a, b) {
        (Ok(output), Err(err)) => {
            assert_eq!(output, json!({}));
            assert!(err.to_string().contains("already exists"));
            first
        }
        (Err(err), Ok(output)) => {
            assert_eq!(output, json!({}));
            assert!(err.to_string().contains("already exists"));
            second
        }
        _ => panic!("exactly one writer must succeed"),
    };
    let notes = root.join("extensions/ad_hoc/notes");
    assert_eq!(
        std::fs::read_to_string(notes.join(filename)).unwrap(),
        expected
    );
    assert_eq!(std::fs::read_dir(notes).unwrap().count(), 1);
}

#[tokio::test]
async fn list_pages_filter_hidden_entries_and_advance_zero_limits() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a.md", "b.md", "c.md", ".hidden"] {
        std::fs::write(root.path().join(name), "text").unwrap();
    }
    let backend = LocalMemoriesBackend::from_memory_root(root.path());
    let direct = backend
        .list(crate::backend::ListMemoriesRequest {
            path: None,
            cursor: None,
            max_results: 0,
        })
        .await
        .unwrap();
    assert_eq!(direct.entries.len(), 1);
    assert_eq!(direct.entries[0].path, "a.md");
    assert_eq!(direct.next_cursor.as_deref(), Some("1"));
    let direct_search = backend
        .search(crate::backend::SearchMemoriesRequest {
            queries: vec!["text".into()],
            match_mode: crate::backend::SearchMatchMode::Any,
            path: None,
            cursor: None,
            context_lines: 0,
            case_sensitive: true,
            normalized: false,
            max_results: 0,
        })
        .await
        .unwrap();
    assert_eq!(direct_search.matches.len(), 1);
    assert_eq!(direct_search.matches[0].path, "a.md");
    assert_eq!(direct_search.next_cursor.as_deref(), Some("1"));
    let first = invoke_memory_tool(
        root.path(),
        crate::LIST_TOOL_NAME,
        json!({"max_results":0}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(
        first["entries"],
        json!([{"path":"a.md", "entry_type":"file"}])
    );
    assert_eq!(first["next_cursor"], json!("1"));
    let last = invoke_memory_tool(
        root.path(),
        crate::LIST_TOOL_NAME,
        json!({"max_results":2,"cursor":"1"}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(
        last["entries"],
        json!([{"path":"b.md", "entry_type":"file"},{"path":"c.md", "entry_type":"file"}])
    );
    assert_eq!(last["next_cursor"], json!(null));
    assert_eq!(last["truncated"], json!(false));
    let err = invoke_memory_tool(
        root.path(),
        crate::LIST_TOOL_NAME,
        json!({"path":"missing"}).to_string(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, codex_extension_api::FunctionCallError::RespondToModel(message) if message.contains("was not found"))
    );
}

#[tokio::test]
async fn tool_arguments_accept_empty_defaults_and_reject_duplicate_fields() {
    let root = tempfile::tempdir().unwrap();
    let result = invoke_memory_tool(root.path(), crate::LIST_TOOL_NAME, "  ".into())
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"path":null,"entries":[],"next_cursor":null,"truncated":false})
    );
    let err = invoke_memory_tool(
        root.path(),
        crate::READ_TOOL_NAME,
        r#"{"path":"a.md","path":"b.md"}"#.into(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, codex_extension_api::FunctionCallError::RespondToModel(message) if message.contains("duplicate field"))
    );
}
