use super::*;
use crate::session::step_context::StepContext;
use pretty_assertions::assert_eq;

#[test]
fn only_stable_locate_task_fragments_force_a_canonical_artifact() {
    let stable = ToolOutputProjectionFragment::new(
        ToolOutputProjectionFragmentKind::SourcePrimaryImplementation,
        "source",
    )
    .with_id("section-id");
    let anonymous = ToolOutputProjectionFragment::new(
        ToolOutputProjectionFragmentKind::SourcePrimaryImplementation,
        "source",
    );

    assert!(requires_canonical_projection_artifact(
        "locate_task",
        std::slice::from_ref(&stable),
    ));
    assert!(!requires_canonical_projection_artifact(
        "locate_task",
        &[anonymous],
    ));
    assert!(!requires_canonical_projection_artifact(
        "search_source",
        std::slice::from_ref(&stable),
    ));
    assert!(requires_canonical_projection_artifact(
        "read_file_span",
        &[stable],
    ));
}

#[test]
fn complete_skill_source_read_uses_bounded_four_thousand_token_projection() {
    let body = "complete skill instruction coverage ".repeat(280);
    let output = format!(
        "Source file evidence:\ncitation: C:\\skills\\repo-atlas\\SKILL.md:1-126\ntotal_lines: 126 bytes_returned: {} truncated: false\nsource_route: core\n{body}",
        body.len()
    );
    assert!(approx_token_count(&output) > 1_000);
    assert!(approx_token_count(&output) < COMPLETE_SKILL_READ_TOKEN_LIMIT);

    let requested_limit = skill_read_projection_limit("read_file_span", &output);
    assert_eq!(requested_limit, Some(COMPLETE_SKILL_READ_TOKEN_LIMIT));
    assert_eq!(
        skill_read_projection_limit("functions.read_file_span", &output),
        Some(COMPLETE_SKILL_READ_TOKEN_LIMIT)
    );
    let limits = resolve_projected_output_limits(
        requested_limit,
        OutputOutcome::Success,
        OutputDiagnosticClass::Normal,
        DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
    );
    assert_eq!(limits.applied_limit, COMPLETE_SKILL_READ_TOKEN_LIMIT);
    let projected = formatted_truncate_text_with_output_limit(&output, limits);

    assert!(!projected.was_truncated);
    assert_eq!(projected.text, output);

    let wrapped = format!("Script completed\nWall time 0.1 seconds\nOutput:\n\n{output}");
    assert_eq!(
        skill_read_projection_limit("functions.exec", &wrapped),
        Some(COMPLETE_SKILL_READ_TOKEN_LIMIT)
    );
    let escaped = wrapped.replace('\n', "\\n");
    assert_eq!(
        skill_read_projection_limit("functions.exec", &escaped),
        Some(COMPLETE_SKILL_READ_TOKEN_LIMIT)
    );
}

#[test]
fn ordinary_and_oversized_skill_outputs_keep_safe_projection_behavior() {
    assert_eq!(
        skill_read_projection_limit("read_file_span", "ordinary tool output"),
        None
    );
    assert_eq!(
        skill_read_projection_limit(
            "read_file_span",
            "citation: /skills/repo-atlas/SKILL.md:1-126\nSource file evidence:\nordinary output"
        ),
        None
    );
    let complete_envelope = "Source file evidence:\ncitation: /skills/repo-atlas/SKILL.md:1-126\ntotal_lines: 126 bytes_returned: 10 truncated: false\nsource_route: core\ninstructions";
    assert_eq!(
        skill_read_projection_limit("mcp__untrusted__tool", complete_envelope),
        None
    );
    let ordinary_limits = resolve_projected_output_limits(
        None,
        OutputOutcome::Success,
        OutputDiagnosticClass::Normal,
        usize::MAX,
    );
    assert_eq!(ordinary_limits.applied_limit, 1_000);

    let oversized = format!(
        "Source file evidence:\ncitation: /skills/large/SKILL.md:1-900\ntotal_lines: 900 bytes_returned: 24000 truncated: false\nsource_route: core\n{}",
        "large skill instruction ".repeat(1_000)
    );
    let skill_limits = resolve_projected_output_limits(
        skill_read_projection_limit("read_file_span", &oversized),
        OutputOutcome::Success,
        OutputDiagnosticClass::Normal,
        usize::MAX,
    );
    let projected = formatted_truncate_text_with_output_limit(&oversized, skill_limits);
    assert_eq!(skill_limits.applied_limit, COMPLETE_SKILL_READ_TOKEN_LIMIT);
    assert!(projected.was_truncated);
}

#[test]
fn complete_projection_envelope_respects_applied_limit() {
    let envelope = ToolProjectionV1 {
        version: 1,
        tool: "test".to_string(),
        outcome: "success".to_string(),
        canonical_sha256: "hash".to_string(),
        canonical_bytes: 8_000,
        canonical_approximate_tokens: 2_000,
        canonical_complete: true,
        model_bytes: 0,
        model_approximate_tokens: 0,
        artifact_id: Some("artifact-123".to_string()),
        sections: Vec::new(),
        omitted_sections: Vec::new(),
        result: serde_json::json!({}),
    };
    let output = "!".repeat(8_000);

    let (projected, rendered) =
        serialize_projection_with_limit(envelope, &output, 64).expect("projection");

    assert!(approx_token_count(&rendered) <= 64);
    assert_eq!(
        serde_json::from_str::<Value>(&rendered).expect("valid JSON projection"),
        projected
    );
}

#[test]
fn typed_projection_prioritizes_sections_with_stable_exact_deduplication() {
    let citation_one = ToolOutputProjectionFragment::new(
        ToolOutputProjectionFragmentKind::CitationOrExactSpan,
        "citation-one",
    );
    let fragments = vec![
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ContextualSpillableText,
            "context-one",
        ),
        citation_one.clone(),
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
            "error-one",
        ),
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::CitationOrExactSpan,
            "citation-two",
        ),
        citation_one,
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::SearchMatchOrDefinition,
            "match-one",
        ),
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary,
            "validation-one",
        ),
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ProcessFinalStatus,
            "status-one",
        ),
    ];

    let (projected, facts) = select_typed_projection_fragments(&fragments, 500);

    assert!(approx_token_count(&projected) <= 500);
    assert_eq!(
        facts,
        ProjectionSelectionFacts {
            mode: "typed_fragments",
            available_fragments: 8,
            selected_fragments: 7,
            exact_duplicates_removed: 1,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        }
    );
    assert_eq!(projected.matches("citation-one").count(), 1);
    assert!(
        projected.find("citation-one").expect("first citation")
            < projected.find("citation-two").expect("second citation")
    );
    assert!(
        projected
            .find("[citations and exact spans]")
            .expect("citation section")
            < projected
                .find("[errors and diagnostics]")
                .expect("diagnostic section")
    );
    assert!(
        projected
            .find("[process final status]")
            .expect("status section")
            < projected.find("[context]").expect("context section")
    );
}

#[test]
fn typed_projection_fairly_bounds_each_nonempty_section() {
    let fragments = PROJECTION_FRAGMENT_KIND_ORDER
        .into_iter()
        .map(|kind| ToolOutputProjectionFragment::new(kind, "section payload ".repeat(100)))
        .collect::<Vec<_>>();

    let (projected, facts) = select_typed_projection_fragments(&fragments, 120);

    assert!(approx_token_count(&projected) <= 120);
    assert_eq!(facts.selected_fragments, fragments.len());
    for kind in PROJECTION_FRAGMENT_KIND_ORDER {
        assert!(projected.contains(fragment_section_heading(kind)));
    }
}

#[test]
fn structured_fixtures_stay_within_baseline_budget_and_reduce_recovery() {
    let fixtures = [
        (
            format!(
                "{}\ncitation: src/owner.rs:40-44\ndefinition: owner_symbol\n{}",
                "irrelevant discovery context ".repeat(300),
                "unrelated search tail ".repeat(300),
            ),
            vec![
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::CitationOrExactSpan,
                    "citation: src/owner.rs:40-44",
                ),
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::SearchMatchOrDefinition,
                    "definition: owner_symbol",
                ),
            ],
            ["citation: src/owner.rs:40-44", "definition: owner_symbol"],
        ),
        (
            format!(
                "{}\nerror: expected Ready, found Pending\nvalidation failed: cache ownership\n{}",
                "irrelevant validation setup ".repeat(300),
                "unrelated compiler context ".repeat(300),
            ),
            vec![
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
                    "error: expected Ready, found Pending",
                ),
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary,
                    "validation failed: cache ownership",
                ),
            ],
            [
                "error: expected Ready, found Pending",
                "validation failed: cache ownership",
            ],
        ),
        (
            format!(
                "{}\nprocess final status: exit 1\ncitation: tests/race.rs:9-12\n{}",
                "irrelevant process output ".repeat(300),
                "unrelated terminal tail ".repeat(300),
            ),
            vec![
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                    "process final status: exit 1",
                ),
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::CitationOrExactSpan,
                    "citation: tests/race.rs:9-12",
                ),
            ],
            [
                "process final status: exit 1",
                "citation: tests/race.rs:9-12",
            ],
        ),
    ];
    let limits = resolve_projected_output_limits(
        Some(120),
        OutputOutcome::Success,
        OutputDiagnosticClass::Normal,
        usize::MAX,
    );
    let mut baseline_tokens = 0;
    let mut structured_tokens = 0;
    let mut baseline_recovery_reads = 0;
    let mut structured_recovery_reads = 0;

    for (full_output, fragments, required) in fixtures {
        let generic = formatted_truncate_text_with_output_limit(&full_output, limits);
        let (structured, _) = select_typed_projection_fragments(&fragments, limits.applied_limit);
        let generic_tokens = approx_token_count(&generic.text);
        let fixture_structured_tokens = approx_token_count(&structured);
        let generic_recoveries = required
            .iter()
            .filter(|needle| !generic.text.contains(*needle))
            .count();
        let fixture_structured_recoveries = required
            .iter()
            .filter(|needle| !structured.contains(*needle))
            .count();

        assert!(generic.was_truncated);
        assert!(fixture_structured_tokens <= limits.applied_limit);
        assert_eq!(fixture_structured_recoveries, 0);
        if fixture_structured_tokens > generic_tokens {
            assert!(fixture_structured_recoveries < generic_recoveries);
        }
        baseline_tokens += generic_tokens;
        structured_tokens += fixture_structured_tokens;
        baseline_recovery_reads += generic_recoveries;
        structured_recovery_reads += fixture_structured_recoveries;
    }

    assert!(structured_tokens * 100 <= baseline_tokens * 115);
    assert!(structured_recovery_reads < baseline_recovery_reads);
}

#[tokio::test]
async fn structured_projection_artifact_recovers_original_bytes() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let thread_id = "structured-projection-thread";
    let full_output = format!(
        "first line\n{}\nfinal status: failed\n",
        "context that must remain recoverable ".repeat(200),
    );
    let fragments = vec![
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
            "error: focused failure",
        ),
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ProcessFinalStatus,
            "final status: failed",
        ),
    ];
    let (projected_text, selection_facts) = select_typed_projection_fragments(&fragments, 200);
    let canonical = CanonicalToolResult::text(full_output.clone());
    let original_output_sha256 = canonical.sha256.clone();
    let original_output_tokens = canonical.approximate_tokens;

    let projection = project_model_output(ModelProjectionInput {
        spillable_text: full_output.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "structured-call".to_string(),
        selection_facts,
        applied_token_limit: 200,
        projected_text,
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: thread_id.to_string(),
        tool_name: "test".to_string(),
        canonical,
        original_output_sha256,
        original_output_tokens,
        semantic_class: "test".to_string(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: Vec::new(),
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "structured-call".to_string(),
            output: FunctionCallOutputPayload::from_text(full_output.clone()),
        },
    })
    .await
    .expect("structured projection");
    let artifact_id = projection.candidate.artifact_id;
    let recovered = crate::tools::command_output_artifact::read_tool_output_artifact(
        temp.path(),
        thread_id,
        &artifact_id,
        1,
        100,
        16_384,
    )
    .await
    .expect("artifact recovery");
    let (_, recovered_payload) = recovered
        .split_once('\n')
        .expect("artifact metadata line and payload");

    assert_eq!(recovered_payload.as_bytes(), full_output.as_bytes());
}

#[test]
fn predetermined_artifact_range_validation_is_exact_and_fails_open() {
    let valid = vec![
        ToolOutputProjectionRange {
            id: "head".to_string(),
            start_line: 1,
            end_line: 64,
        },
        ToolOutputProjectionRange {
            id: "middle".to_string(),
            start_line: 100,
            end_line: 163,
        },
        ToolOutputProjectionRange {
            id: "tail".to_string(),
            start_line: 229,
            end_line: 300,
        },
    ];
    assert_eq!(validated_predetermined_ranges(&valid), valid);

    let mut overlapping = valid;
    overlapping[1].start_line = 64;
    assert!(validated_predetermined_ranges(&overlapping).is_empty());

    let oversized = vec![ToolOutputProjectionRange {
        id: "oversized".to_string(),
        start_line: 1,
        end_line: 201,
    }];
    assert!(validated_predetermined_ranges(&oversized).is_empty());
    assert!(validated_predetermined_ranges(&[]).is_empty());
}

#[tokio::test]
async fn three_predetermined_artifact_ranges_are_drained_in_original_return() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let thread_id = "predetermined-range-thread";
    let full_output = (1..=300)
        .map(|line| format!("line-{line:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let canonical = CanonicalToolResult::text(full_output.clone());
    let ranges = vec![
        ToolOutputProjectionRange {
            id: "head".to_string(),
            start_line: 1,
            end_line: 2,
        },
        ToolOutputProjectionRange {
            id: "middle".to_string(),
            start_line: 150,
            end_line: 151,
        },
        ToolOutputProjectionRange {
            id: "tail".to_string(),
            start_line: 299,
            end_line: 300,
        },
    ];
    let original_output_sha256 = canonical.sha256.clone();
    let original_output_tokens = canonical.approximate_tokens;
    let projection = project_model_output(ModelProjectionInput {
        spillable_text: full_output.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "predetermined-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "test",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 4_000,
        projected_text: "bounded validation summary".to_string(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: thread_id.to_string(),
        tool_name: "shell_command".to_string(),
        canonical,
        original_output_sha256,
        original_output_tokens,
        semantic_class: "validation".to_string(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: ranges,
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "predetermined-call".to_string(),
            output: FunctionCallOutputPayload::from_text(full_output),
        },
    })
    .await
    .expect("projection");

    let rendered = match projection.response {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            output.body.to_text().expect("projected text")
        }
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(rendered.contains("Host-drained predetermined artifact ranges"));
    for expected in [
        "line-001", "line-002", "line-150", "line-151", "line-299", "line-300",
    ] {
        assert!(rendered.contains(expected), "missing {expected}");
    }
    let receipt = projection
        .deterministic_continuation_receipt
        .expect("artifact-range receipt");
    assert_eq!(receipt.class, DeterministicContinuationClass::ArtifactRange);
    assert_eq!(receipt.suppressed_continuation_count, 3);
    assert_eq!(receipt.avoided_token_usage, None);
}

#[test]
fn direct_mcp_projection_keeps_error_metadata_and_non_text_modalities() {
    let original = ResponseInputItem::McpToolCallOutput {
        call_id: "mcp-call".to_string(),
        output: codex_protocol::mcp::CallToolResult {
            content: vec![
                serde_json::json!({"type": "text", "text": "large original text"}),
                serde_json::json!({"type": "image", "data": "image-bytes", "mimeType": "image/png"}),
            ],
            structured_content: Some(serde_json::json!({"large": "structured value"})),
            is_error: Some(true),
            meta: Some(serde_json::json!({"provider": "test"})),
        },
    };

    let projected = projected_response_item(original, r#"{"version":1}"#.to_string());
    let ResponseInputItem::McpToolCallOutput { call_id, output } = projected else {
        panic!("expected MCP response");
    };
    assert_eq!(call_id, "mcp-call");
    assert_eq!(output.is_error, Some(true));
    assert_eq!(output.meta, Some(serde_json::json!({"provider": "test"})));
    assert_eq!(output.structured_content, None);
    assert_eq!(
        output.content,
        vec![
            serde_json::json!({"type": "text", "text": r#"{"version":1}"#}),
            serde_json::json!({"type": "image", "data": "image-bytes", "mimeType": "image/png"}),
        ],
    );
}

#[test]
fn json_projection_metadata_keeps_generic_fallback_by_default() {
    let metadata = codex_tools::ToolOutputProjectionMetadata::from_json(
        &serde_json::json!({"text": "x"}),
        true,
        None,
    );

    assert!(metadata.fragments.is_empty());
    assert_eq!(metadata.spillable_text, vec![r#"{"text":"x"}"#.to_string()]);
}

struct TestHandler {
    tool_name: codex_tools::ToolName,
}

impl ToolExecutor<ToolInvocation> for TestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Ok(
                Box::new(crate::tools::context::FunctionToolOutput::from_text(
                    "ok".to_string(),
                    Some(true),
                )) as Box<dyn crate::tools::context::ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for TestHandler {}

#[derive(Clone)]
enum LifecycleTestResult {
    Ok { success: bool },
    Err,
}

struct LifecycleTestHandler {
    tool_name: codex_tools::ToolName,
    result: LifecycleTestResult,
}

impl ToolExecutor<ToolInvocation> for LifecycleTestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call())
    }
}

impl LifecycleTestHandler {
    async fn handle_call(
        &self,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        match self.result.clone() {
            LifecycleTestResult::Ok { success } => Ok(Box::new(
                crate::tools::context::FunctionToolOutput::from_text(
                    "ok".to_string(),
                    Some(success),
                ),
            )
                as Box<dyn crate::tools::context::ToolOutput>),
            LifecycleTestResult::Err => Err(FunctionCallError::RespondToModel(
                "handler failed".to_string(),
            )),
        }
    }
}

impl CoreToolRuntime for LifecycleTestHandler {}

fn test_spec(tool_name: &codex_tools::ToolName) -> codex_tools::ToolSpec {
    codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
        name: tool_name.name.clone(),
        description: "Test tool.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: codex_tools::JsonSchema::default(),
        output_schema: None,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum RecordedToolLifecycle {
    Start {
        call_id: String,
        tool_name: codex_tools::ToolName,
    },
    Finish {
        call_id: String,
        tool_name: codex_tools::ToolName,
        outcome: codex_extension_api::ToolCallOutcome,
    },
}

struct ToolLifecycleRecorder {
    records: Arc<std::sync::Mutex<Vec<RecordedToolLifecycle>>>,
}

impl codex_extension_api::ToolLifecycleContributor for ToolLifecycleRecorder {
    fn on_tool_start<'a>(
        &'a self,
        input: codex_extension_api::ToolStartInput<'a>,
    ) -> codex_extension_api::ToolLifecycleFuture<'a> {
        let records = Arc::clone(&self.records);
        let record = RecordedToolLifecycle::Start {
            call_id: input.call_id.to_string(),
            tool_name: input.tool_name.clone(),
        };
        Box::pin(async move {
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
        })
    }

    fn on_tool_finish<'a>(
        &'a self,
        input: codex_extension_api::ToolFinishInput<'a>,
    ) -> codex_extension_api::ToolLifecycleFuture<'a> {
        let records = Arc::clone(&self.records);
        let record = RecordedToolLifecycle::Finish {
            call_id: input.call_id.to_string(),
            tool_name: input.tool_name.clone(),
            outcome: input.outcome,
        };
        Box::pin(async move {
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
        })
    }
}

#[test]
fn handler_looks_up_namespaced_aliases_explicitly() {
    let namespace = "mcp__codex_apps__gmail";
    let tool_name = "gmail_get_recent_emails";
    let plain_name = codex_tools::ToolName::plain(tool_name);
    let namespaced_name = codex_tools::ToolName::namespaced(namespace, tool_name);
    let plain_handler = Arc::new(TestHandler {
        tool_name: plain_name.clone(),
    }) as Arc<dyn CoreToolRuntime>;
    let namespaced_handler = Arc::new(TestHandler {
        tool_name: namespaced_name.clone(),
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::new(HashMap::from([
        (plain_name.clone(), Arc::clone(&plain_handler)),
        (namespaced_name.clone(), Arc::clone(&namespaced_handler)),
    ]));

    let plain = registry.tool(&plain_name);
    let namespaced = registry.tool(&namespaced_name);
    let missing_namespaced = registry.tool(&codex_tools::ToolName::namespaced(
        "mcp__codex_apps__calendar",
        tool_name,
    ));

    assert_eq!(plain.is_some(), true);
    assert_eq!(namespaced.is_some(), true);
    assert_eq!(missing_namespaced.is_none(), true);
    assert!(
        plain
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &plain_handler))
    );
    assert!(
        namespaced
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &namespaced_handler))
    );
}

#[tokio::test]
async fn function_tools_expose_default_hook_payloads_and_rewrites() -> anyhow::Result<()> {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let tool_name = codex_tools::ToolName::namespaced("functions.", "echo");
    let handler = TestHandler {
        tool_name: tool_name.clone(),
    };
    let invocation = ToolInvocation {
        payload: ToolPayload::Function {
            arguments: serde_json::json!({ "message": "hello" }).to_string(),
        },
        ..test_invocation(Arc::new(session), Arc::new(turn), "call-1", tool_name)
    };
    let output =
        crate::tools::context::FunctionToolOutput::from_text("echoed".to_string(), Some(true));

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::new("functions.echo"),
            tool_input: serde_json::json!({ "message": "hello" }),
        })
    );
    assert_eq!(
        handler.post_tool_use_payload(&invocation, &output),
        Some(PostToolUsePayload {
            tool_name: HookToolName::new("functions.echo"),
            tool_use_id: "call-1".to_string(),
            tool_input: serde_json::json!({ "message": "hello" }),
            tool_response: serde_json::json!("echoed"),
        })
    );

    let invocation = handler
        .with_updated_hook_input(invocation, serde_json::json!({ "message": "rewritten" }))?;
    let ToolPayload::Function { arguments } = invocation.payload else {
        panic!("generic rewritten function payload should remain function-shaped");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&arguments)?,
        serde_json::json!({ "message": "rewritten" })
    );

    Ok(())
}

#[tokio::test]
async fn function_hook_input_defaults_empty_arguments_to_object() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let tool_name = codex_tools::ToolName::plain("echo");
    let handler = TestHandler {
        tool_name: tool_name.clone(),
    };
    let invocation = ToolInvocation {
        payload: ToolPayload::Function {
            arguments: "  ".to_string(),
        },
        ..test_invocation(Arc::new(session), Arc::new(turn), "call-1", tool_name)
    };

    assert_eq!(
        handler.pre_tool_use_payload(&invocation),
        Some(PreToolUsePayload {
            tool_name: HookToolName::new("echo"),
            tool_input: serde_json::json!({}),
        })
    );
}

#[tokio::test]
async fn spawn_agent_function_tools_use_agent_matcher_alias() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let hook_payloads = [
        codex_tools::ToolName::plain("spawn_agent"),
        codex_tools::ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "spawn_agent"),
    ]
    .into_iter()
    .map(|tool_name| {
        let handler = TestHandler {
            tool_name: tool_name.clone(),
        };
        let invocation = ToolInvocation {
            payload: ToolPayload::Function {
                arguments: serde_json::json!({ "message": "inspect this repo" }).to_string(),
            },
            ..test_invocation(Arc::clone(&session), Arc::clone(&turn), "call-1", tool_name)
        };
        handler.pre_tool_use_payload(&invocation)
    })
    .collect::<Vec<_>>();

    assert_eq!(
        hook_payloads,
        vec![
            Some(PreToolUsePayload {
                tool_name: HookToolName::spawn_agent(),
                tool_input: serde_json::json!({ "message": "inspect this repo" }),
            }),
            Some(PreToolUsePayload {
                tool_name: HookToolName::spawn_agent(),
                tool_input: serde_json::json!({ "message": "inspect this repo" }),
            }),
        ]
    );
}

#[tokio::test]
async fn code_mode_wait_does_not_expose_default_hook_payloads() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let output = crate::tools::context::FunctionToolOutput::from_text("ok".to_string(), Some(true));

    let wait = crate::tools::handlers::CodeModeWaitHandler;
    let wait_invocation = test_invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait-call",
        wait.tool_name(),
    );
    assert_eq!(wait.pre_tool_use_payload(&wait_invocation), None);
    assert_eq!(wait.post_tool_use_payload(&wait_invocation, &output), None);
}

#[tokio::test]
async fn write_stdin_does_not_expose_default_pre_tool_use_payload() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;

    let write_stdin = crate::tools::handlers::WriteStdinHandler;
    let invocation = test_invocation(
        Arc::new(session),
        Arc::new(turn),
        "write-stdin-call",
        write_stdin.tool_name(),
    );

    assert_eq!(write_stdin.pre_tool_use_payload(&invocation), None);
    assert!(write_stdin.supports_parallel_tool_calls());
}

#[test]
fn post_tool_use_feedback_output_keeps_code_mode_result_typed() {
    let result = AnyToolResult {
        call_id: "call-1".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        result: Box::new(PostToolUseFeedbackOutput {
            original: Box::new(codex_tools::JsonToolOutput::new(
                serde_json::json!({ "typed": true }),
            )),
            model_visible: crate::tools::context::FunctionToolOutput::from_text(
                "hook feedback".to_string(),
                /*success*/ None,
            ),
        }),
        post_tool_use_payload: None,
        model_projection: None,
    };

    assert_eq!(
        result.into_response(),
        ResponseInputItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "hook feedback".to_string()
            ),
        }
    );

    let result = AnyToolResult {
        call_id: "call-1".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        result: Box::new(PostToolUseFeedbackOutput {
            original: Box::new(codex_tools::JsonToolOutput::new(
                serde_json::json!({ "typed": true }),
            )),
            model_visible: crate::tools::context::FunctionToolOutput::from_text(
                "hook feedback".to_string(),
                /*success*/ None,
            ),
        }),
        post_tool_use_payload: None,
        model_projection: None,
    };

    assert_eq!(
        result.code_mode_result(),
        serde_json::json!({ "typed": true })
    );
}

#[tokio::test]
async fn dispatch_notifies_tool_lifecycle_contributors() -> anyhow::Result<()> {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.tool_lifecycle_contributor(Arc::new(ToolLifecycleRecorder {
        records: Arc::clone(&records),
    }));
    session.services.extensions = Arc::new(builder.build());

    let ok_tool = codex_tools::ToolName::plain("ok_tool");
    let failing_tool = codex_tools::ToolName::plain("failing_tool");
    let ok_handler = Arc::new(LifecycleTestHandler {
        tool_name: ok_tool.clone(),
        result: LifecycleTestResult::Ok { success: false },
    }) as Arc<dyn CoreToolRuntime>;
    let failing_handler = Arc::new(LifecycleTestHandler {
        tool_name: failing_tool.clone(),
        result: LifecycleTestResult::Err,
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::new(HashMap::from([
        (ok_tool.clone(), ok_handler),
        (failing_tool.clone(), failing_handler),
    ]));
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    registry
        .dispatch_any(test_invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "ok-call",
            ok_tool.clone(),
        ))
        .await?;
    let err = match registry
        .dispatch_any(test_invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "failing-call",
            failing_tool.clone(),
        ))
        .await
    {
        Ok(_) => panic!("failing handler should return an error"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "handler failed");

    let expected = vec![
        RecordedToolLifecycle::Start {
            call_id: "ok-call".to_string(),
            tool_name: ok_tool.clone(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "ok-call".to_string(),
            tool_name: ok_tool,
            outcome: codex_extension_api::ToolCallOutcome::Completed { success: false },
        },
        RecordedToolLifecycle::Start {
            call_id: "failing-call".to_string(),
            tool_name: failing_tool.clone(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "failing-call".to_string(),
            tool_name: failing_tool,
            outcome: codex_extension_api::ToolCallOutcome::Failed {
                handler_executed: true,
            },
        },
    ];
    let actual = records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect::<Vec<_>>();
    assert_eq!(expected, actual);

    Ok(())
}

fn test_invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    call_id: &str,
    tool_name: codex_tools::ToolName,
) -> ToolInvocation {
    let step_context = StepContext::for_test(Arc::clone(&turn));
    ToolInvocation {
        session,
        step_context,
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: call_id.to_string(),
        tool_name,
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }
}
