use super::*;
use codex_exec_server::LOCAL_FS;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::SearchToolCallParams;
use codex_utils_path_uri::PathUri;
use core_test_support::assert_regex_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn custom_tool_calls_should_roundtrip_as_custom_outputs() {
    let payload = ToolPayload::Custom {
        input: "patch".to_string(),
    };
    let response = FunctionToolOutput::from_text("patched".to_string(), Some(true))
        .to_response_item("call-42", &payload);

    match response {
        ResponseInputItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            assert_eq!(call_id, "call-42");
            assert_eq!(output.content_items(), None);
            assert_eq!(output.body.to_text().as_deref(), Some("patched"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected CustomToolCallOutput, got {other:?}"),
    }
}

#[tokio::test]
async fn apply_patch_output_returns_ordered_structured_result() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("structured.txt");
    let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
    let action = codex_apply_patch::ApplyPatchAction::new_add_for_test(
        &path_uri,
        "structured\n".to_string(),
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let delta = action
        .execute(
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .expect("execute patch");
    let output = ApplyPatchToolOutput::from_execution(
        "Success. Updated files.".to_string(),
        true,
        &action,
        &delta,
    );
    let payload = ToolPayload::Custom {
        input: action.patch.clone(),
    };
    let result = output.code_mode_result(&payload);

    assert_eq!(result["status"], "completed");
    assert_eq!(result["exact"], true);
    assert_eq!(result["operations"].as_array().map(Vec::len), Some(1));
    assert_eq!(result["committed_delta"].as_array().map(Vec::len), Some(1));
    assert_eq!(result["operations"][0]["operation"], "add");
    assert_eq!(result["committed_delta"][0]["operation"], "add");
    assert_eq!(
        output.post_tool_use_response("call-1", &payload),
        Some(result.clone())
    );

    let response = output.to_response_item("call-1", &payload);
    let ResponseInputItem::CustomToolCallOutput { output, .. } = response else {
        panic!("expected custom tool output");
    };
    assert_eq!(output.success, Some(true));
    assert_eq!(
        output.body.to_text().as_deref(),
        Some("Success. Updated files.")
    );

    let partial = ApplyPatchToolOutput::from_execution(
        "Failed after a committed prefix.".to_string(),
        false,
        &action,
        &delta,
    );
    assert_eq!(partial.code_mode_result(&payload)["status"], "partial");
    assert!(!partial.success_for_logging());
}

#[test]
fn apply_patch_output_distinguishes_no_op_and_failed() {
    let dir = tempdir().expect("tempdir");
    let path =
        PathUri::from_host_native_path(&dir.path().join("status.txt")).expect("absolute test path");
    let action = codex_apply_patch::ApplyPatchAction::new_add_for_test(&path, "new".to_string());
    let empty = codex_apply_patch::AppliedPatchDelta::default();

    let no_op =
        ApplyPatchToolOutput::from_execution("No changes.".to_string(), true, &action, &empty);
    let failed =
        ApplyPatchToolOutput::from_execution("Failed.".to_string(), false, &action, &empty);
    assert_eq!(
        no_op.code_mode_result(&ToolPayload::Custom {
            input: String::new()
        })["status"],
        "no_op"
    );
    assert_eq!(
        failed.code_mode_result(&ToolPayload::Custom {
            input: String::new()
        })["status"],
        "failed"
    );
    assert!(no_op.success_for_logging());
    assert!(!failed.success_for_logging());
}

#[test]
fn apply_patch_output_treats_empty_inexact_failure_as_partial() {
    assert_eq!(
        apply_patch_status(
            /*execution_succeeded*/ false, /*delta_is_empty*/ true,
            /*delta_is_exact*/ false,
        ),
        "partial"
    );
}

#[test]
fn function_payloads_remain_function_outputs() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let response = FunctionToolOutput::from_text("ok".to_string(), Some(true))
        .to_response_item("fn-1", &payload);

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "fn-1");
            assert_eq!(output.content_items(), None);
            assert_eq!(output.body.to_text().as_deref(), Some("ok"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_code_mode_result_serializes_full_call_tool_result() {
    let output = CallToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": "ignored",
        })],
        structured_content: Some(serde_json::json!({
            "threadId": "thread_123",
            "content": "done",
        })),
        is_error: Some(false),
        meta: Some(serde_json::json!({
            "source": "mcp",
        })),
    };

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(
        result,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": "ignored",
            }],
            "structuredContent": {
                "threadId": "thread_123",
                "content": "done",
            },
            "isError": false,
            "_meta": {
                "source": "mcp",
            },
        })
    );
}

#[test]
fn mcp_tool_output_response_item_includes_wall_time() {
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "done",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        tool_input: json!({}),
        wall_time: std::time::Duration::from_millis(1250),
        original_image_detail_supported: false,
        truncation_policy: TruncationPolicy::Bytes(1024),
    };

    let response = output.to_response_item(
        "mcp-call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "mcp-call-1");
            assert_eq!(output.success, Some(true));
            let Some(text) = output.body.to_text() else {
                panic!("MCP output should serialize as text");
            };
            let Some(payload) = text.strip_prefix("Wall time: 1.2500 seconds\nOutput:\n") else {
                panic!("MCP output should include wall-time header: {text}");
            };
            let parsed: serde_json::Value = serde_json::from_str(payload).unwrap_or_else(|err| {
                panic!("MCP output should serialize JSON content: {err}");
            });
            assert_eq!(
                parsed,
                json!([{
                    "type": "text",
                    "text": "done",
                }])
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_tool_output_response_item_truncates_large_structured_content() {
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "ignored when structured content is present",
            })],
            structured_content: Some(serde_json::json!({
                "items": "large structured value ".repeat(1_000),
            })),
            is_error: Some(false),
            meta: None,
        },
        tool_input: json!({}),
        wall_time: std::time::Duration::from_millis(1250),
        original_image_detail_supported: false,
        truncation_policy: TruncationPolicy::Bytes(128),
    };

    let response = output.to_response_item(
        "mcp-call-large",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "mcp-call-large");
            assert_eq!(output.success, Some(true));
            let text = output
                .body
                .to_text()
                .expect("MCP output should serialize as text");
            assert!(text.starts_with("Wall time: 1.2500 seconds\nOutput:\n"));
            assert!(text.contains("chars truncated"));
            assert!(!text.contains("ignored when structured content is present"));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_tool_output_response_item_preserves_content_items() {
    let image_url = "data:image/png;base64,AAA";
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "AAA",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        tool_input: json!({}),
        wall_time: std::time::Duration::from_millis(500),
        original_image_detail_supported: false,
        truncation_policy: TruncationPolicy::Bytes(1024),
    };

    let response = output.to_response_item(
        "mcp-call-2",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    match response {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(
                output.content_items(),
                Some(
                    vec![
                        FunctionCallOutputContentItem::InputText {
                            text: "Wall time: 0.5000 seconds\nOutput:".to_string(),
                        },
                        FunctionCallOutputContentItem::InputImage {
                            image_url: image_url.to_string(),
                            detail: Some(DEFAULT_IMAGE_DETAIL),
                        },
                    ]
                    .as_slice()
                )
            );
            assert_eq!(
                output.body.to_text().as_deref(),
                Some("Wall time: 0.5000 seconds\nOutput:")
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn mcp_tool_output_code_mode_result_stays_raw_call_tool_result() {
    let large_content = "large structured value ".repeat(1_000);
    let output = McpToolOutput {
        result: CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "ignored",
            })],
            structured_content: Some(serde_json::json!({
                "content": large_content,
            })),
            is_error: Some(false),
            meta: None,
        },
        tool_input: json!({}),
        wall_time: std::time::Duration::from_millis(1250),
        original_image_detail_supported: false,
        truncation_policy: TruncationPolicy::Bytes(64),
    };

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(
        result,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": "ignored",
            }],
            "structuredContent": {
                "content": "large structured value ".repeat(1_000),
            },
            "isError": false,
        })
    );
}

#[test]
fn custom_tool_calls_can_derive_text_from_content_items() {
    let payload = ToolPayload::Custom {
        input: "patch".to_string(),
    };
    let response = FunctionToolOutput::from_content(
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "line 1".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAA".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText {
                text: "line 2".to_string(),
            },
        ],
        Some(true),
    )
    .to_response_item("call-99", &payload);

    match response {
        ResponseInputItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            let expected = vec![
                FunctionCallOutputContentItem::InputText {
                    text: "line 1".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAA".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "line 2".to_string(),
                },
            ];
            assert_eq!(call_id, "call-99");
            assert_eq!(output.content_items(), Some(expected.as_slice()));
            assert_eq!(output.body.to_text().as_deref(), Some("line 1\nline 2"));
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected CustomToolCallOutput, got {other:?}"),
    }
}

#[test]
fn tool_search_payloads_roundtrip_as_tool_search_outputs() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };
    let response = ToolSearchOutput {
        tools: vec![LoadableToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "create_event".to_string(),
            description: String::new(),
            strict: false,
            defer_loading: Some(true),
            parameters: codex_tools::JsonSchema::object(
                /*properties*/ Default::default(),
                /*required*/ None,
                /*additional_properties*/ None,
            ),
            output_schema: None,
        })],
    }
    .to_response_item("search-1", &payload);

    match response {
        ResponseInputItem::ToolSearchOutput {
            call_id,
            status,
            execution,
            tools,
        } => {
            assert_eq!(call_id, "search-1");
            assert_eq!(status, "completed");
            assert_eq!(execution, "client");
            assert_eq!(
                tools,
                vec![json!({
                    "type": "function",
                    "name": "create_event",
                    "description": "",
                    "strict": false,
                    "defer_loading": true,
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                })]
            );
        }
        other => panic!("expected ToolSearchOutput, got {other:?}"),
    }
}

#[test]
fn log_preview_uses_content_items_when_plain_text_is_missing() {
    let output = FunctionToolOutput::from_content(
        vec![FunctionCallOutputContentItem::InputText {
            text: "preview".to_string(),
        }],
        Some(true),
    );

    assert_eq!(output.log_preview(), "preview");
    assert_eq!(
        function_call_output_content_items_to_text(&output.body),
        Some("preview".to_string())
    );
}

#[test]
fn telemetry_preview_returns_original_within_limits() {
    let content = "short output";
    assert_eq!(telemetry_preview(content), content);
}

#[test]
fn telemetry_preview_truncates_by_bytes() {
    let content = "x".repeat(TELEMETRY_PREVIEW_MAX_BYTES + 8);
    let preview = telemetry_preview(&content);

    assert!(preview.contains(TELEMETRY_PREVIEW_TRUNCATION_NOTICE));
    assert!(
        preview.len()
            <= TELEMETRY_PREVIEW_MAX_BYTES + TELEMETRY_PREVIEW_TRUNCATION_NOTICE.len() + 1
    );
}

#[test]
fn telemetry_preview_truncates_by_lines() {
    let content = (0..(TELEMETRY_PREVIEW_MAX_LINES + 5))
        .map(|idx| format!("line {idx}"))
        .collect::<Vec<_>>()
        .join("\n");

    let preview = telemetry_preview(&content);
    let lines: Vec<&str> = preview.lines().collect();

    assert!(lines.len() <= TELEMETRY_PREVIEW_MAX_LINES + 1);
    assert_eq!(lines.last(), Some(&TELEMETRY_PREVIEW_TRUNCATION_NOTICE));
}

#[test]
fn exec_command_tool_output_formats_truncated_response() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let response = ExecCommandToolOutput {
        event_call_id: "call-42".to_string(),
        chunk_id: "abc123".to_string(),
        wall_time: std::time::Duration::from_millis(1250),
        raw_output: b"token one token two token three token four token five".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(4),
        process_id: None,
        exit_code: Some(0),
        timed_out: false,
        original_token_count: Some(10),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
        analysis: Default::default(),
    }
    .to_response_item("call-42", &payload);

    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call-42");
            assert_eq!(output.success, Some(true));
            let text = output
                .body
                .to_text()
                .expect("exec output should serialize as text");
            assert_regex_match(
                r#"(?sx)
                    ^Chunk\ ID:\ abc123
                    \nWall\ time:\ \d+\.\d{4}\ seconds
                    \nProcess\ exited\ with\ code\ 0
                    \nOriginal\ token\ count:\ 10
                    \nOutput:
                    \n.*\[\+\d+B/\d+L\].*
                    $"#,
                &text,
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn exec_command_tool_output_summarizes_and_links_retained_raw_output() {
    let raw_output = (0..900)
        .map(|index| {
            if index == 450 {
                format!("error: exact retained failure marker {index}")
            } else {
                format!("ordinary-{index:04}-{}", "x".repeat(72))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let artifact_path = std::path::PathBuf::from(r"C:\codex\tool-output\raw.log");
    let output = ExecCommandToolOutput {
        event_call_id: "call-summary".to_string(),
        chunk_id: "chunk-summary".to_string(),
        wall_time: std::time::Duration::from_millis(25),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(1),
        timed_out: false,
        original_token_count: Some(20_000),
        hook_command: Some("cargo test".to_string()),
        raw_output_artifact: Some(RawOutputArtifact::Stored {
            path: artifact_path.clone(),
            bytes: raw_output.len() as u64,
        }),
        repair_notice: Some("Command preflight applied one repair".to_string()),
        analysis: Default::default(),
    };

    let response = output.response_text();
    assert!(response.contains("Shell output summary:"));
    assert!(response.contains("error: exact retained failure marker 450"));
    assert!(!response.contains("ordinary-0300"));
    assert!(response.contains(&artifact_path.display().to_string()));
    assert!(response.contains("Command preflight applied one repair"));

    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(
        code_mode["raw_output_artifact"],
        artifact_path.to_string_lossy().as_ref()
    );
    assert_eq!(code_mode["raw_output_artifact_bytes"], raw_output.len());
    assert_eq!(code_mode["outcome"], "completed_failure");
    assert_eq!(code_mode["timed_out"], false);
    assert!(code_mode.get("chunk_id").is_none());
    assert!(code_mode.get("wall_time_seconds").is_none());
    let mut replay = output.clone();
    replay.chunk_id = "different-transport-chunk".to_string();
    replay.wall_time = std::time::Duration::from_secs(99);
    assert_eq!(
        code_mode,
        replay.code_mode_result(&ToolPayload::Function {
            arguments: "{}".to_string(),
        })
    );
    assert!(!output.success_for_logging());
    let response_item = output.to_response_item(
        "call-summary",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    assert!(matches!(
        response_item,
        ResponseInputItem::FunctionCallOutput { output, .. }
            if output.success == Some(false)
    ));

    let mut running = output.clone();
    running.process_id = Some(42);
    running.exit_code = None;
    assert_eq!(running.outcome(), crate::exec::ExecCommandOutcome::Running);
    assert!(!running.timed_out);
    running.timed_out = true;
    assert_eq!(running.outcome(), crate::exec::ExecCommandOutcome::TimedOut);
    assert!(
        code_mode["output"]
            .as_str()
            .is_some_and(|value| value.contains("Shell output summary:"))
    );
}

#[test]
fn exec_command_tool_output_clones_share_cached_analysis() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "call-cache".to_string(),
        chunk_id: "chunk-cache".to_string(),
        wall_time: std::time::Duration::from_millis(5),
        raw_output: b"alpha beta gamma delta".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(3),
        process_id: None,
        exit_code: Some(0),
        timed_out: false,
        original_token_count: Some(4),
        hook_command: Some("echo alpha".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
        analysis: Default::default(),
    };
    let cloned = output.clone();

    let hook_response = output.post_tool_use_response("call-cache", &payload);
    let preview = output.log_preview();
    let response = output.to_response_item("call-cache", &payload);
    let code_mode = output.code_mode_result(&payload);

    assert!(std::sync::Arc::ptr_eq(&output.analysis, &cloned.analysis));
    assert_eq!(
        output.decoded_output().as_ptr(),
        cloned.decoded_output().as_ptr()
    );
    assert_eq!(output.hook_output().as_ptr(), cloned.hook_output().as_ptr());
    assert_eq!(
        output.model_output().as_ptr(),
        cloned.model_output().as_ptr()
    );
    assert_eq!(
        output.response_text().as_ptr(),
        cloned.response_text().as_ptr()
    );
    assert_eq!(output.preview().as_ptr(), cloned.preview().as_ptr());
    assert_eq!(
        hook_response,
        cloned.post_tool_use_response("call-cache", &payload)
    );
    assert_eq!(preview, cloned.log_preview());
    assert_eq!(response, cloned.to_response_item("call-cache", &payload));
    assert_eq!(code_mode, cloned.code_mode_result(&payload));
}
