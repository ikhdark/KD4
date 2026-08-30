use super::*;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::SearchToolCallParams;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn orchestration_audit_tool_dispatch_state_has_one_terminal_transition_owner() {
    let completed = ToolDispatchState::new();
    assert!(completed.try_admit());
    assert!(completed.try_complete());
    assert_eq!(completed.try_abort(), ToolDispatchAbort::AlreadyTerminal);
    assert!(completed.is_terminal());
    assert!(!completed.is_aborted());

    let cancelled_before_admission = ToolDispatchState::new();
    assert_eq!(
        cancelled_before_admission.try_abort(),
        ToolDispatchAbort::BeforeAdmission
    );
    assert!(!cancelled_before_admission.try_admit());
    assert!(!cancelled_before_admission.try_complete());
    assert!(cancelled_before_admission.is_aborted());

    let cancelled_after_admission = ToolDispatchState::new();
    assert!(cancelled_after_admission.try_admit());
    assert_eq!(
        cancelled_after_admission.try_abort(),
        ToolDispatchAbort::AfterAdmission
    );
    assert!(!cancelled_after_admission.try_complete());
    assert!(cancelled_after_admission.is_aborted());
}

fn mcp_tool_output(
    result: CallToolResult,
    wall_time: std::time::Duration,
    original_image_detail_supported: bool,
    truncation_policy: TruncationPolicy,
) -> McpToolOutput {
    McpToolOutput::new(
        result,
        json!({}),
        wall_time,
        original_image_detail_supported,
        truncation_policy,
    )
}

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
fn omitted_function_output_status_is_not_reported_as_success() {
    let output = FunctionToolOutput::from_text("status unavailable".to_string(), None);

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(!output.success_for_logging());
    assert_eq!(
        output
            .projection_metadata()
            .map(|metadata| metadata.outcome),
        Some(ToolOutputOutcome::Failure)
    );
}

#[test]
fn apply_patch_code_mode_result_preserves_output() {
    let text = "Success. Updated the following files:\nA code_mode_apply_patch.txt\n".to_string();
    let output = ApplyPatchToolOutput::from_text(text.clone());

    assert_eq!(
        output.code_mode_result(&ToolPayload::Function {
            arguments: "{}".to_string(),
        }),
        json!(text)
    );
}

#[test]
fn skipped_function_outputs_remain_typed_and_non_successful() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let neutral = FunctionToolOutput::from_text("not run".to_string(), Some(true))
        .with_outcome(ToolOutputOutcome::Skipped);
    assert_eq!(
        neutral.outcome_context(),
        ToolOutputOutcomeContext::skipped(None)
    );
    assert!(!neutral.success_for_logging());
    match neutral.to_response_item("skip-neutral", &payload) {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(output.success, Some(false));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }

    let deferred = FunctionToolOutput::from_text("later".to_string(), Some(true))
        .with_skip_disposition(ToolOutputSkipDisposition::Deferred);
    assert_eq!(
        deferred.outcome_context(),
        ToolOutputOutcomeContext::skipped(Some(ToolOutputSkipDisposition::Deferred))
    );
    assert!(!deferred.success_for_logging());
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

fn assert_mcp_wrapper_preserves_projection_metadata(
    result: CallToolResult,
    expected_outcome: ToolOutputOutcome,
    expected_diagnostic_class: ToolOutputDiagnosticClass,
) {
    let native = ToolOutput::projection_metadata(&result).expect("native MCP metadata");
    let wrapped = mcp_tool_output(
        result,
        std::time::Duration::from_millis(25),
        false,
        TruncationPolicy::Bytes(1024),
    )
    .projection_metadata()
    .expect("wrapped MCP metadata");

    assert_eq!(wrapped.outcome, expected_outcome);
    assert_eq!(wrapped.diagnostic_class, expected_diagnostic_class);
    assert_eq!(wrapped.outcome, native.outcome);
    assert_eq!(wrapped.diagnostic_class, native.diagnostic_class);
    assert_eq!(wrapped.fragments, native.fragments);
    assert_eq!(wrapped.spillable_text, native.spillable_text);
    assert_eq!(wrapped.essential_inline, native.essential_inline);
    assert_eq!(wrapped.requested_limit, native.requested_limit);
    assert_eq!(wrapped.predetermined_ranges, native.predetermined_ranges);
    assert_eq!(
        wrapped.predetermined_json_pointers,
        native.predetermined_json_pointers
    );

    let limits_for = |metadata: &ToolOutputProjectionMetadata| {
        let outcome = match metadata.outcome {
            ToolOutputOutcome::Success => OutputOutcome::Success,
            ToolOutputOutcome::Failure => OutputOutcome::Failure,
            ToolOutputOutcome::TimedOut => OutputOutcome::TimedOut,
            ToolOutputOutcome::Yielded => OutputOutcome::Success,
            ToolOutputOutcome::Skipped => OutputOutcome::Skipped,
        };
        let diagnostic_class = match metadata.diagnostic_class {
            ToolOutputDiagnosticClass::Normal => {
                codex_utils_output_truncation::OutputDiagnosticClass::Normal
            }
            ToolOutputDiagnosticClass::HighSignal => {
                codex_utils_output_truncation::OutputDiagnosticClass::HighSignal
            }
        };
        resolve_projected_output_limits(metadata.requested_limit, outcome, diagnostic_class, 4_000)
    };
    assert_eq!(limits_for(&wrapped), limits_for(&native));
}

#[test]
fn mcp_wrapper_preserves_native_success_projection_metadata() {
    assert_mcp_wrapper_preserves_projection_metadata(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "provider success",
            })],
            structured_content: Some(serde_json::json!({"value": 42})),
            is_error: Some(false),
            meta: Some(serde_json::json!({"provider": "fixture"})),
        },
        ToolOutputOutcome::Success,
        ToolOutputDiagnosticClass::Normal,
    );
}

#[test]
fn mcp_wrapper_preserves_native_high_signal_projection_metadata() {
    let result = CallToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": "provider failure",
        })],
        structured_content: Some(serde_json::json!({"code": "provider_failed"})),
        is_error: Some(true),
        meta: Some(serde_json::json!({"provider": "fixture"})),
    };
    let native = ToolOutput::projection_metadata(&result).expect("native MCP metadata");
    assert_mcp_wrapper_preserves_projection_metadata(
        result,
        ToolOutputOutcome::Failure,
        ToolOutputDiagnosticClass::HighSignal,
    );
    assert!(
        native.fragments.iter().any(|fragment| {
            fragment.kind == ToolOutputProjectionFragmentKind::ErrorOrDiagnostic
        })
    );
    let applied = resolve_projected_output_limits(
        native.requested_limit,
        OutputOutcome::Failure,
        codex_utils_output_truncation::OutputDiagnosticClass::HighSignal,
        4_000,
    );
    assert_eq!(applied.applied_limit, 4_000);
}

#[test]
fn mcp_tool_output_response_item_includes_wall_time() {
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "done",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(1250),
        false,
        TruncationPolicy::Bytes(1024),
    );

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
fn mcp_sampling_identity_excludes_wall_time() {
    let result = CallToolResult {
        content: vec![json!({ "type": "text", "text": "done" })],
        structured_content: None,
        is_error: Some(false),
        meta: None,
    };
    let output = |wall_time| {
        mcp_tool_output(
            result.clone(),
            wall_time,
            false,
            TruncationPolicy::Bytes(1024),
        )
    };

    assert_eq!(
        output(std::time::Duration::from_millis(1)).sampling_request_signal(),
        output(std::time::Duration::from_secs(9)).sampling_request_signal(),
    );
}

#[test]
fn confirmed_performance_mcp_output_reuses_raw_and_provider_projections() {
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![json!({ "type": "text", "text": "done" })],
            structured_content: Some(json!({ "value": 42 })),
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(25),
        false,
        TruncationPolicy::Bytes(1024),
    );
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };

    assert_eq!(output.projection_cache_state(), (false, false));
    let expected_raw = output.code_mode_result(&payload);
    assert_eq!(output.sampling_request_signal().is_some(), true);
    assert_eq!(
        output.post_tool_use_response("call", &payload),
        Some(expected_raw)
    );
    assert_eq!(output.projection_cache_state(), (true, false));

    let cloned = output.clone();
    let _ = output.log_preview();
    let _ = output.to_response_item("call", &payload);
    assert_eq!(output.projection_cache_state(), (true, true));
    assert_eq!(cloned.projection_cache_state(), (true, true));
}

#[test]
fn mcp_tool_output_response_item_truncates_large_structured_content() {
    let output = mcp_tool_output(
        CallToolResult {
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
        std::time::Duration::from_millis(1250),
        false,
        TruncationPolicy::Bytes(128),
    );

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
    let output = mcp_tool_output(
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "AAA",
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
        std::time::Duration::from_millis(500),
        false,
        TruncationPolicy::Bytes(1024),
    );

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
    let output = mcp_tool_output(
        CallToolResult {
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
        std::time::Duration::from_millis(1250),
        false,
        TruncationPolicy::Bytes(64),
    );

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
fn function_output_with_image_uses_complete_json_canonical_result() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let output = FunctionToolOutput::from_content(
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "caption".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAA".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ],
        Some(true),
    );

    let canonical = output
        .canonical_result(&payload)
        .expect("canonical function output");
    let value: serde_json::Value =
        serde_json::from_slice(&canonical.bytes).expect("canonical JSON");

    assert!(value.to_string().contains("data:image/png;base64,AAA"));
    assert!(canonical.complete);
}

#[test]
fn confirmed_performance_single_text_function_output_uses_direct_canonical_text() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let output = FunctionToolOutput::from_text("exact text".to_string(), Some(true));
    FunctionToolOutput::reset_projection_metadata_call_count();

    let canonical = output
        .canonical_result(&payload)
        .expect("canonical function output");

    assert_eq!(canonical.bytes, b"exact text".to_vec());
    assert!(canonical.complete);
    assert_eq!(FunctionToolOutput::projection_metadata_call_count(), 0);
}

#[test]
fn tool_search_payloads_roundtrip_as_tool_search_outputs() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };
    let output = ToolSearchOutput {
        tools: vec![json!({
            "type": "function",
            "name": "create_event",
            "description": "",
            "strict": false,
            "defer_loading": true,
            "parameters": {
                "type": "object",
                "properties": {}
            }
        })],
        omitted_result_count: 0,
    };
    assert_eq!(
        output.code_mode_result(&payload),
        json!({
            "status": "completed",
            "execution": "client",
            "tools": [{
                "type": "function",
                "name": "create_event",
                "description": "",
                "strict": false,
                "defer_loading": true,
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }],
            "omitted_result_count": 0,
        })
    );
    let response = output.to_response_item("search-1", &payload);

    match response {
        ResponseInputItem::ToolSearchOutput {
            call_id,
            status,
            execution,
            tools,
            omitted_result_count,
        } => {
            assert_eq!(call_id, "search-1");
            assert_eq!(status, "completed");
            assert_eq!(execution, "client");
            assert_eq!(omitted_result_count, Some(0));
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
fn partial_tool_search_outputs_are_model_visible_as_incomplete() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };
    let output = ToolSearchOutput {
        tools: Vec::new(),
        omitted_result_count: 1,
    };
    assert_eq!(
        output.code_mode_result(&payload),
        json!({
            "status": "incomplete",
            "execution": "client",
            "tools": [],
            "omitted_result_count": 1,
        })
    );
    let response = output.to_response_item("search-partial", &payload);

    match response {
        ResponseInputItem::ToolSearchOutput {
            status,
            tools,
            omitted_result_count,
            ..
        } => {
            assert_eq!(status, "incomplete");
            assert!(tools.is_empty());
            assert_eq!(omitted_result_count, Some(1));
        }
        other => panic!("expected ToolSearchOutput, got {other:?}"),
    }
}

#[test]
fn aborted_tool_search_payloads_preserve_abort_status() {
    let payload = ToolPayload::ToolSearch {
        arguments: SearchToolCallParams {
            query: "calendar".to_string(),
            limit: None,
        },
    };

    let output = AbortedToolOutput {
        message: "cancelled".to_string(),
    };
    assert_eq!(
        output.code_mode_result(&payload),
        json!({
            "status": "aborted",
            "execution": "client",
            "tools": [],
            "omitted_result_count": null,
        })
    );
    assert_eq!(
        output.to_response_item("search-aborted", &payload),
        ResponseInputItem::ToolSearchOutput {
            call_id: "search-aborted".to_string(),
            status: "aborted".to_string(),
            execution: "client".to_string(),
            tools: Vec::new(),
            omitted_result_count: None,
        }
    );
}

#[test]
fn ordinary_aborted_code_mode_output_is_structured() {
    let output = AbortedToolOutput {
        message: "cancelled".to_string(),
    };

    assert_eq!(
        output.code_mode_result(&ToolPayload::Function {
            arguments: "{}".to_string(),
        }),
        json!({
            "status": "aborted",
            "message": "cancelled",
        })
    );
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
fn command_semantic_evidence_normalizes_read_only_presentations() {
    let source_fact = "let stable = compute();";
    let presentations = [
        source_fact.to_string(),
        format!("src/lib.rs:10:{source_fact}"),
        format!("SOURCEMAP.md:494:{source_fact}"),
        format!("diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+{source_fact}"),
        format!("  --> src/lib.rs:10:1\n10 | {source_fact}\n   | ^^^"),
    ];
    let expected = semantic_evidence_for_command_output(presentations[0].as_bytes());
    for presentation in &presentations[1..] {
        assert_eq!(
            semantic_evidence_for_command_output(presentation.as_bytes()),
            expected
        );
        assert_eq!(
            command_failure_signature(
                &semantic_evidence_for_command_output(presentation.as_bytes()),
                Some(1)
            ),
            command_failure_signature(&expected, Some(1))
        );
    }
    assert_ne!(
        semantic_evidence_for_command_output(b"let changed = compute();"),
        expected
    );
}

#[test]
fn command_semantic_evidence_preserves_diagnostics_and_non_location_numbers() {
    let source = "10 | let stable = compute();";
    let first_diagnostic = format!("error[E0001]: first failure\n{source}");
    let second_diagnostic = format!("error[E0002]: second failure\n{source}");
    assert_ne!(
        semantic_evidence_for_command_output(first_diagnostic.as_bytes()),
        semantic_evidence_for_command_output(second_diagnostic.as_bytes())
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"service-a:8080: healthy"),
        semantic_evidence_for_command_output(b"service-b:9090: healthy")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"12 failures remain"),
        semantic_evidence_for_command_output(b"13 failures remain")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"https://service-a:8080: healthy"),
        semantic_evidence_for_command_output(b"https://service-b:8080: healthy")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"db.example.com:5432: ready"),
        semantic_evidence_for_command_output(b"cache.example.com:5432: ready")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"src/lib.rs:10:8080: healthy"),
        semantic_evidence_for_command_output(b"src/lib.rs:10:9090: healthy")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"running 5 workers"),
        semantic_evidence_for_command_output(b"running 6 workers")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"let value = \"a  b\";"),
        semantic_evidence_for_command_output(b"let value = \"a b\";")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"10 | legitimate table value"),
        semantic_evidence_for_command_output(b"legitimate table value")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"fact\n}"),
        semantic_evidence_for_command_output(b"fact\n]")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"first fact\nsecond fact"),
        semantic_evidence_for_command_output(b"second fact\nfirst fact")
    );
    assert_ne!(
        semantic_evidence_for_command_output(b"Ok"),
        semantic_evidence_for_command_output(b"No")
    );
    assert_ne!(
        semantic_evidence_for_command_output(&[0xff, b'a']),
        semantic_evidence_for_command_output("�a".as_bytes())
    );
    assert_ne!(
        semantic_evidence_for_command_output(
            b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+same fact\nfatal: first"
        ),
        semantic_evidence_for_command_output(
            b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+same fact\nfatal: second"
        )
    );
}

#[test]
fn command_semantic_evidence_preserves_removed_and_context_diff_lines() {
    let first_removal =
        b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old value\n+new value";
    let second_removal =
        b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-other old value\n+new value";
    assert_ne!(
        semantic_evidence_for_command_output(first_removal),
        semantic_evidence_for_command_output(second_removal)
    );

    let first_context = b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1,2 +1,2 @@\n first context\n-old value\n+new value";
    let second_context = b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -1,2 +1,2 @@\n second context\n-old value\n+new value";
    assert_ne!(
        semantic_evidence_for_command_output(first_context),
        semantic_evidence_for_command_output(second_context)
    );
}

#[test]
fn ansi_stripping_preserves_text_after_a_non_csi_escape() {
    assert_eq!(strip_ansi_sequences("before\u{1b}Xafter"), "beforeXafter");
    assert_eq!(
        strip_ansi_sequences("before\u{1b}[31mred\u{1b}[0mafter"),
        "beforeredafter"
    );
}

#[test]
fn command_semantic_evidence_includes_facts_after_the_old_limit() {
    let shared = (0..512)
        .map(|index| format!("shared fact {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let first = format!("{shared}\nfirst tail fact");
    let second = format!("{shared}\nsecond tail fact");
    assert_ne!(
        semantic_evidence_for_command_output(first.as_bytes()),
        semantic_evidence_for_command_output(second.as_bytes())
    );

    let long_prefix = "x".repeat(4_096);
    assert_ne!(
        semantic_evidence_for_command_output(format!("{long_prefix} first").as_bytes()),
        semantic_evidence_for_command_output(format!("{long_prefix} second").as_bytes())
    );
}

#[test]
fn command_semantic_evidence_preserves_fact_multiplicity() {
    assert_ne!(
        semantic_evidence_for_command_output(b"same fact\nsame fact"),
        semantic_evidence_for_command_output(b"same fact")
    );
}

#[test]
fn command_failure_signature_preserves_exit_status() {
    let evidence = semantic_evidence_for_command_output(b"same diagnostic");
    assert_ne!(
        command_failure_signature(&evidence, Some(1)),
        command_failure_signature(&evidence, Some(2))
    );
}

#[test]
fn token_efficiency_exec_output_omits_redundant_headers() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let response = ExecCommandToolOutput {
        event_call_id: "call-42".to_string(),
        chunk_id: "abc123".to_string(),
        wall_time: std::time::Duration::from_millis(1250),
        raw_output: vec![b'x'; 400],
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(20),
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: Some(100),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
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
            assert!(text.starts_with("Process exited with code 0; wall time: 1.2500 seconds"));
            assert!(!text.contains("Chunk ID:"));
            assert!(!text.contains("Original token count:"));
            assert!(codex_utils_string::approx_token_count(&text) <= 20);
            assert_ne!(
                text,
                String::from_utf8(vec![b'x'; 400]).expect("UTF-8 fixture")
            );
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn retained_exec_command_process_is_yielded_not_timed_out() {
    let output = ExecCommandToolOutput {
        event_call_id: "retained-call".to_string(),
        chunk_id: "retained-chunk".to_string(),
        wall_time: std::time::Duration::from_millis(250),
        raw_output: b"process still running".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: None,
        process_id: Some(4242),
        exit_code: None,
        process_exited: false,
        original_token_count: Some(3),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
    };

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Yielded);
    assert!(output.success_for_logging());
    assert_eq!(
        output.model_output_max_tokens(),
        codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS
    );
    assert_eq!(
        output
            .projection_metadata()
            .expect("retained output should have projection metadata")
            .outcome,
        ToolOutputOutcome::Yielded
    );
    match output.to_response_item(
        "retained-call",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    ) {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(output.success, Some(true));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn tool_result_correctness_missing_exit_code_is_not_reported_as_success() {
    let output = ExecCommandToolOutput {
        event_call_id: "missing-exit-call".to_string(),
        chunk_id: "missing-exit-chunk".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: b"exit status unavailable".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(1_000),
        process_id: None,
        exit_code: None,
        process_exited: true,
        original_token_count: Some(3),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
    };

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(!output.success_for_logging());
    assert!(
        output
            .response_text()
            .contains("Process exited without an available exit code")
    );
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(code_mode["process_exited"], json!(true));
    assert_eq!(code_mode["exit_code"], JsonValue::Null);
}

#[test]
fn tool_result_correctness_exited_process_with_pending_output_is_not_live() {
    let output = ExecCommandToolOutput {
        event_call_id: "pending-output-call".to_string(),
        chunk_id: "pending-output-chunk".to_string(),
        wall_time: std::time::Duration::from_millis(10),
        raw_output: b"remaining output".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(1_000),
        process_id: Some(4242),
        exit_code: Some(7),
        process_exited: true,
        original_token_count: Some(2),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
    };

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    let metadata = output
        .projection_metadata()
        .expect("exec output should expose projection metadata");
    assert_eq!(metadata.essential_inline["session_id"], json!(4242));
    assert_eq!(metadata.essential_inline["exit_code"], json!(7));
    assert_eq!(metadata.essential_inline["process_exited"], json!(true));
    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(code_mode["process_exited"], json!(true));
    assert!(
        output
            .response_text()
            .contains("Process exited with code 7")
    );
    assert!(!output.response_text().contains("Process running"));
}

#[test]
fn exec_command_projection_metadata_preserves_authoritative_first_output() {
    let raw_output = "first output line\n".repeat(100);
    let output = ExecCommandToolOutput {
        event_call_id: "call-first-output".to_string(),
        chunk_id: "chunk-first-output".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(20),
        process_id: Some(42),
        exit_code: None,
        process_exited: false,
        original_token_count: Some(300),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
    };

    let metadata = output
        .projection_metadata()
        .expect("exec output should expose projection metadata");

    assert_eq!(metadata.spillable_text, vec![raw_output.clone()]);
    assert_eq!(
        metadata.fragments,
        vec![
            ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                "process final status: exit_code=None, session_id=Some(42), wall_time_seconds=0.0010",
            )
            .with_id("process_status"),
            ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ContextualSpillableText,
                raw_output,
            )
            .with_id("output")
        ]
    );
    assert_eq!(metadata.essential_inline["session_id"], json!(42));
    assert_eq!(metadata.essential_inline["exit_code"], JsonValue::Null);
    assert_eq!(metadata.essential_inline["wall_time_seconds"], json!(0.001));
    assert_eq!(metadata.essential_inline["repair_notice"], JsonValue::Null);
}

#[test]
fn token_efficiency_exec_projection_reports_truncation_once() {
    let output = ExecCommandToolOutput {
        event_call_id: "call-hard-limit".to_string(),
        chunk_id: "chunk-hard-limit".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: vec![b'x'; 400],
        truncation_policy: TruncationPolicy::Tokens(5),
        max_output_tokens: Some(20),
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: Some(100),
        hook_command: Some("echo ok".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };

    let raw_output = String::from_utf8_lossy(&output.raw_output);
    let projected = output.projected_model_output(raw_output.as_ref());
    assert!(projected.reduced);
    assert_eq!(
        projected.text.matches("Warning: truncated output").count(),
        1
    );
    assert!(!projected.text.contains("tokens truncated"));
}

#[test]
fn exec_command_projection_reports_reduction_from_per_call_limit() {
    let output = ExecCommandToolOutput {
        event_call_id: "call-per-call-limit".to_string(),
        chunk_id: "chunk-per-call-limit".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: b"token one token two token three token four token five".to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(4),
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: Some(10),
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
    };

    let raw_output = String::from_utf8_lossy(&output.raw_output);
    let projected = output.projected_model_output(raw_output.as_ref());
    assert!(projected.reduced);
    assert!(!projected.text.is_empty());
    assert!(codex_utils_string::approx_token_count(&projected.text) <= 4);
}

#[test]
fn token_backfire_unified_exec_keeps_complete_output_that_fits_budget() {
    let raw_output = (0..700)
        .map(|index| format!("line-{index}: exact evidence"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = ExecCommandToolOutput {
        event_call_id: "call-complete-output".to_string(),
        chunk_id: "chunk-complete-output".to_string(),
        wall_time: std::time::Duration::from_millis(1),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(20_000),
        max_output_tokens: Some(20_000),
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: Some(codex_utils_string::approx_token_count(&raw_output)),
        hook_command: Some("enumerate evidence".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    };

    let projected = output.projected_model_output(&raw_output);

    assert!(!projected.reduced);
    assert_eq!(projected.text, raw_output);
}

#[test]
fn high_signal_validation_exposes_three_bounded_predetermined_ranges() {
    let raw_output = (1..=300)
        .map(|line| format!("error[E0001]: focused diagnostic line {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let ranges = predetermined_validation_ranges(&raw_output, Some("cargo test -p focused"));

    assert_eq!(
        ranges,
        vec![
            ToolOutputProjectionRange {
                id: "validation-head".to_string(),
                start_line: 1,
                end_line: 64,
            },
            ToolOutputProjectionRange {
                id: "validation-middle".to_string(),
                start_line: 119,
                end_line: 182,
            },
            ToolOutputProjectionRange {
                id: "validation-tail".to_string(),
                start_line: 229,
                end_line: 300,
            },
        ]
    );
    assert_eq!(
        ranges
            .iter()
            .map(|range| range.end_line - range.start_line + 1)
            .sum::<usize>(),
        200
    );
}

#[test]
fn predetermined_validation_ranges_are_absent_when_not_needed() {
    let ordinary = (1..=300)
        .map(|line| format!("ordinary output {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(predetermined_validation_ranges(&ordinary, Some("echo ok")).is_empty());

    let short_diagnostic = (1..=200)
        .map(|line| format!("error: focused diagnostic {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        predetermined_validation_ranges(&short_diagnostic, Some("cargo check -p focused"))
            .is_empty()
    );
}

#[test]
fn token_efficiency_exec_output_preserves_live_process_state_for_large_output() {
    let raw_output = (0..900)
        .map(|index| format!("live-process-output-{index:04}-{}", "x".repeat(72)))
        .collect::<Vec<_>>()
        .join("\n");
    let response = ExecCommandToolOutput {
        event_call_id: "call-live".to_string(),
        chunk_id: "chunk-live".to_string(),
        wall_time: std::time::Duration::from_millis(25),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(256),
        process_id: Some(42),
        exit_code: None,
        process_exited: false,
        original_token_count: Some(20_000),
        hook_command: Some("cargo test".to_string()),
        raw_output_artifact: None,
        repair_notice: None,
    }
    .response_text();

    assert!(response.contains("Process running with session ID 42"));
    assert!(!response.contains("Process exited with code"));
    assert!(!response.contains("exit_code: 0"));
    assert!(!response.contains("timed_out: true"));
    assert_eq!(response.matches("Warning: truncated output").count(), 1);
    assert!(response.len() < raw_output.len());
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
    let artifact_id: ToolOutputArtifactId = "019fa782-f8e1-7533-a3f7-60d3f9a42997".parse().unwrap();
    let artifact_path =
        std::path::PathBuf::from(format!(r"C:\codex\tool-output\{artifact_id}.log"));
    let output = ExecCommandToolOutput {
        event_call_id: "call-summary".to_string(),
        chunk_id: "chunk-summary".to_string(),
        wall_time: std::time::Duration::from_millis(25),
        raw_output: raw_output.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(1),
        process_exited: true,
        original_token_count: Some(20_000),
        hook_command: Some("cargo test".to_string()),
        raw_output_artifact: Some(RawOutputArtifact::Stored {
            id: artifact_id,
            path: artifact_path.clone(),
            bytes: raw_output.len() as u64,
            truncated: false,
            handle: std::sync::Arc::new(tempfile::tempfile().expect("artifact handle")),
        }),
        repair_notice: Some("Command preflight applied one repair".to_string()),
    };

    let response = output.response_text();
    assert!(
        codex_utils_output_truncation::approx_token_count(&response)
            <= output.model_output_max_tokens()
    );
    assert!(response.contains("Shell output summary:"));
    assert!(response.contains("error: exact retained failure marker 450"));
    assert!(!response.contains("ordinary-0300"));
    assert!(response.contains(&artifact_id.to_string()));
    assert!(!response.contains(&artifact_path.display().to_string()));
    assert!(response.contains("Command preflight applied one repair"));

    let code_mode = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(code_mode["raw_output_artifact_id"], artifact_id.to_string());
    assert_eq!(code_mode["raw_output_artifact_bytes"], raw_output.len());
    assert!(
        code_mode["output"]
            .as_str()
            .is_some_and(|value| value.contains("Shell output summary:"))
    );
}

fn artifact_backed_exec_output(
    raw_output: &[u8],
    max_output_tokens: Option<usize>,
) -> (
    ExecCommandToolOutput,
    ToolOutputArtifactId,
    std::path::PathBuf,
    tempfile::TempDir,
) {
    let artifact_id: ToolOutputArtifactId = "019fa78a-0e8e-78d1-8a9d-b67d330eb5b6".parse().unwrap();
    let retained_root = tempfile::tempdir().expect("retained artifact root");
    let artifact_directory = retained_root.path().join("tool-output").join("thread");
    std::fs::create_dir_all(&artifact_directory).expect("create artifact directory");
    let artifact_path = artifact_directory.join(format!("{artifact_id}.log"));
    std::fs::write(&artifact_path, raw_output).expect("write retained artifact");
    (
        ExecCommandToolOutput {
            event_call_id: "call-artifact".to_string(),
            chunk_id: "chunk-artifact".to_string(),
            wall_time: std::time::Duration::from_millis(1),
            raw_output: raw_output.to_vec(),
            truncation_policy: TruncationPolicy::Tokens(10_000),
            max_output_tokens,
            process_id: None,
            exit_code: Some(0),
            process_exited: true,
            original_token_count: None,
            hook_command: None,
            raw_output_artifact: Some(RawOutputArtifact::Stored {
                id: artifact_id,
                path: artifact_path.clone(),
                bytes: raw_output.len() as u64,
                truncated: false,
                handle: std::sync::Arc::new(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&artifact_path)
                        .expect("artifact handle"),
                ),
            }),
            repair_notice: None,
        },
        artifact_id,
        artifact_path,
        retained_root,
    )
}

#[test]
fn exec_model_output_exposes_artifact_id_not_path() {
    let (output, artifact_id, artifact_path, _retained_root) =
        artifact_backed_exec_output(b"complete output\n", Some(1_000));

    let response = output.response_text();

    assert!(response.contains(&artifact_id.to_string()));
    assert!(!response.contains(&artifact_path.to_string_lossy().to_string()));
}

#[test]
fn exec_code_mode_exposes_artifact_id_not_path() {
    let (mut output, artifact_id, artifact_path, _retained_root) =
        artifact_backed_exec_output(b"complete output\n", Some(1_000));

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(result["raw_output_artifact_id"], artifact_id.to_string());
    assert!(result.get("raw_output_artifact").is_none());
    assert!(
        !result
            .to_string()
            .contains(&artifact_path.to_string_lossy().to_string())
    );

    output.raw_output_artifact = Some(RawOutputArtifact::Failed {
        id: Some(artifact_id),
        message: format!("failed to flush `{}`", artifact_path.display()),
        owned_path: Some(artifact_path.clone()),
        bytes: 7,
    });
    let failed_result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    assert_eq!(
        failed_result["raw_output_artifact_error"],
        "raw output artifact storage failed"
    );
    assert!(
        !failed_result
            .to_string()
            .contains(&artifact_path.to_string_lossy().to_string())
    );
}

#[test]
fn exec_code_mode_makes_empty_completion_explicit() {
    let (mut output, _, _, _retained_root) = artifact_backed_exec_output(b"", Some(1_000));
    output.raw_output_artifact = None;

    let result = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });

    assert_eq!(
        result["output"],
        "Command completed with no output (exit code 0)."
    );
}

#[test]
fn token_efficiency_artifact_recovery_notice_does_not_repeat_id() {
    let raw_output = "word ".repeat(200);
    let (output, artifact_id, _, _retained_root) =
        artifact_backed_exec_output(raw_output.as_bytes(), Some(100));

    let response = output.response_text();

    assert!(response.contains("[command output reduced; recover the full retained output"));
    assert!(response.contains("using the raw output artifact above"));
    assert!(response.contains("do not rerun the producer.]"));
    assert_eq!(response.matches(&artifact_id.to_string()).count(), 1);
}

#[test]
fn exec_reduction_notice_is_absent_for_complete_output() {
    let (output, _, _, _retained_root) =
        artifact_backed_exec_output(b"complete output\n", Some(1_000));

    let response = output.response_text();

    assert!(!response.contains("[command output reduced;"));
}

#[test]
fn exec_reduction_notice_is_absent_after_artifact_is_evicted() {
    let raw_output = "word ".repeat(200);
    let (output, _, artifact_path, _retained_root) =
        artifact_backed_exec_output(raw_output.as_bytes(), Some(4));
    std::fs::remove_file(&artifact_path).expect("evict retained artifact");

    let response = output.response_text();

    assert!(!response.contains("[command output reduced;"));
    assert!(!response.contains("full retained output is available"));

    std::fs::create_dir(&artifact_path).expect("replace artifact with nonregular entry");
    let nonregular_response = output.response_text();
    assert!(!nonregular_response.contains("[command output reduced;"));
    assert!(!nonregular_response.contains("full retained output is available"));
}
