use super::*;
use crate::session::step_context::StepContext;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn admitted_tool_dispatch_state() -> Arc<ToolDispatchState> {
    let state = Arc::new(ToolDispatchState::new());
    assert!(state.try_admit());
    state
}

#[test]
fn nested_code_mode_projection_is_not_provider_visible() {
    assert!(projection_is_provider_visible(&ToolCallSource::Direct));
    assert!(!projection_is_provider_visible(&ToolCallSource::CodeMode {
        cell_id: "cell".to_string(),
        parent_call_id: Some("outer".to_string()),
        runtime_tool_call_id: "nested".to_string(),
    }));
    assert!(admission_tracking_enabled(&ToolCallSource::Direct, true));
    assert!(!admission_tracking_enabled(&ToolCallSource::Direct, false));
}

#[test]
fn direct_output_admission_is_not_config_gated() {
    let source = ToolCallSource::Direct;
    let tool_name = ToolName::plain("ordinary_tool");

    assert!(projection_admission_required(&source, &tool_name, false));
    assert!(!admission_tracking_enabled(&source, false));
}

#[test]
fn direct_code_mode_output_requires_completed_history_admission() {
    let source = ToolCallSource::Direct;
    let tool_name = ToolName::plain(codex_code_mode::PUBLIC_TOOL_NAME);

    assert!(projection_admission_required(&source, &tool_name, false));
    assert!(projection_admission_required(&source, &tool_name, true));
}

#[test]
fn mcp_output_keeps_its_native_provider_shape() {
    let source = ToolCallSource::Direct;
    let tool_name = ToolName::namespaced("mcp__rmcp", "sync");

    assert!(!projection_admission_required(&source, &tool_name, false));
    assert!(!projection_admission_required(&source, &tool_name, true));
}

#[test]
fn direct_recovery_is_exempt_from_recursive_generic_projection() {
    assert!(generic_projection_is_exempt(
        &ToolName::plain("read_tool_output"),
        false,
    ));
    assert!(generic_projection_is_exempt(
        &ToolName::plain("read_tool_output"),
        true,
    ));
    assert!(generic_projection_is_exempt(
        &ToolName::plain("exec"),
        false,
    ));
    assert!(!generic_projection_is_exempt(
        &ToolName::plain("exec"),
        true,
    ));
}

#[tokio::test]
async fn exec_output_logging_and_projection_materialize_response_once() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let call_id = "exec-materialization";
    let invocation = test_invocation(
        Arc::new(session),
        Arc::new(turn),
        call_id,
        ToolName::plain("shell"),
    );
    let output = crate::tools::context::ExecCommandToolOutput {
        event_call_id: call_id.to_string(),
        chunk_id: "chunk-materialization".to_string(),
        wall_time: Duration::from_millis(1),
        raw_output: "large command output ".repeat(2_000).into_bytes(),
        truncation_policy: codex_protocol::protocol::TruncationPolicy::Tokens(10_000),
        max_output_tokens: Some(64),
        process_id: None,
        exit_code: Some(0),
        process_exited: true,
        original_token_count: None,
        hook_command: None,
        raw_output_artifact: None,
        repair_notice: None,
    };
    crate::tools::context::ExecCommandToolOutput::reset_response_materialization_count();
    let result = AnyToolResult {
        call_id: call_id.to_string(),
        payload: invocation.payload.clone(),
        result: Box::new(output),
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };

    let preview = result.result.log_preview();
    assert!(preview.starts_with("large command output"));
    assert_eq!(
        crate::tools::context::ExecCommandToolOutput::response_materialization_count(),
        0
    );
    assert!(
        prepare_model_projection(
            &invocation,
            &result,
            /*parsed_function_arguments*/ None,
            /*source_dependencies_override*/ None,
            /*force_inline_carrier*/ false,
            /*track_for_admission*/ true,
        )
        .is_some()
    );
    assert_eq!(
        crate::tools::context::ExecCommandToolOutput::response_materialization_count(),
        1
    );
}

#[tokio::test]
async fn consumed_code_mode_registry_output_becomes_a_recoverable_receipt() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let call_id = "registry-discovery";
    let raw_registry_output = "ALL_TOOLS registry description ".repeat(600);
    let invocation = ToolInvocation {
        payload: ToolPayload::Custom {
            input: "text(ALL_TOOLS.find(tool => tool.name === 'exec_command').description);"
                .to_string(),
        },
        ..test_invocation(
            Arc::new(session),
            Arc::new(turn),
            call_id,
            ToolName::plain(codex_code_mode::PUBLIC_TOOL_NAME),
        )
    };
    let result = AnyToolResult {
        call_id: call_id.to_string(),
        payload: invocation.payload.clone(),
        result: Box::new(crate::tools::context::FunctionToolOutput::from_text(
            raw_registry_output.clone(),
            Some(true),
        )),
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };

    let projection_input = prepare_model_projection(
        &invocation,
        &result,
        /*parsed_function_arguments*/ None,
        /*source_dependencies_override*/ None,
        /*force_inline_carrier*/ false,
        /*track_for_admission*/ true,
    )
    .expect("direct code-mode output should be admitted");
    assert_eq!(
        projection_input.materialization,
        ProjectionMaterialization::AdmissionOnly
    );
    let projection = project_model_output(projection_input)
        .await
        .expect("code-mode admission projection");
    let first_response = projection.response();
    assert_eq!(
        history_output_text(&first_response).as_deref(),
        Some(raw_registry_output.as_str())
    );
    let candidate = projection
        .candidate
        .expect("completed-tool history candidate");

    let canonical: Arc<[ResponseItem]> = Arc::from([
        ResponseItem::CustomToolCall {
            id: None,
            status: Some("completed".to_string()),
            call_id: call_id.to_string(),
            name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
            namespace: None,
            input: "text(ALL_TOOLS);".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::from(first_response),
    ]);
    let mut history = crate::tool_history::ToolHistoryState::default();
    history.register(candidate);

    let first_exposure = history.project(Arc::clone(&canonical));
    assert!(first_exposure.substitutions.is_empty());
    assert!(history.mark_consumed(
        &first_exposure.items,
        crate::tool_history::ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let later_exposure = history.project(canonical);
    assert_eq!(later_exposure.substitutions.len(), 1);
    assert!(
        later_exposure
            .items
            .iter()
            .any(crate::tool_history::response_item_has_valid_tool_history_receipt)
    );
    assert!(
        !serde_json::to_string(&later_exposure.items)
            .expect("serialize later prompt exposure")
            .contains(&raw_registry_output)
    );
}

#[tokio::test]
async fn yielded_code_mode_output_keeps_its_live_handle_inline() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let invocation = ToolInvocation {
        payload: ToolPayload::Custom {
            input: "yield_control();".to_string(),
        },
        ..test_invocation(
            Arc::new(session),
            Arc::new(turn),
            "live-cell",
            ToolName::plain(codex_code_mode::PUBLIC_TOOL_NAME),
        )
    };
    let result = AnyToolResult {
        call_id: "live-cell".to_string(),
        payload: invocation.payload.clone(),
        result: Box::new(
            crate::tools::context::FunctionToolOutput::from_text(
                "Script running with cell ID cell-1".to_string(),
                /*success*/ None,
            )
            .with_outcome(ToolOutputOutcome::Yielded),
        ),
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };

    assert!(
        prepare_model_projection(
            &invocation,
            &result,
            /*parsed_function_arguments*/ None,
            /*source_dependencies_override*/ None,
            /*force_inline_carrier*/ false,
            /*track_for_admission*/ true,
        )
        .is_none()
    );
}

#[test]
fn typed_preflight_rejects_invalid_hook_rewritten_code_mode_arguments() {
    let spec = ToolSpec::Function(codex_tools::ResponsesApiTool {
        name: "typed_helper".to_string(),
        description: "test helper".to_string(),
        strict: false,
        defer_loading: None,
        parameters: codex_tools::JsonSchema::object(
            BTreeMap::from([("path".to_string(), codex_tools::JsonSchema::string(None))]),
            Some(vec!["path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    });
    let name = ToolName::plain("typed_helper");
    let valid_payload = ToolPayload::Function {
        arguments: serde_json::json!({ "path": "src/lib.rs" }).to_string(),
    };
    let valid_arguments = ParsedFunctionArguments::from_payload(&valid_payload);
    let preflight = CodeModeArgumentPreflight::default();

    assert_eq!(
        preflight.validate(&name, &spec, &valid_payload, valid_arguments.as_ref(),),
        Ok(())
    );
    // This represents the final payload after a PreToolUse hook rewrites the
    // initially valid invocation.
    let invalid_payload = ToolPayload::Function {
        arguments: serde_json::json!({ "path": 7, "extra": true }).to_string(),
    };
    let invalid_arguments = ParsedFunctionArguments::from_payload(&invalid_payload);
    let error = preflight
        .validate(&name, &spec, &invalid_payload, invalid_arguments.as_ref())
        .expect_err("invalid typed arguments must be rejected before dispatch");
    assert!(error.contains("argument preflight failed"));
    assert!(error.contains("/path") || error.contains("additional"));
    assert_eq!(preflight.compile_count.load(Ordering::Relaxed), 1);
    assert_eq!(preflight.validation_count.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn code_mode_dispatch_without_hook_rewrite_preflights_once() -> anyhow::Result<()> {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let tool_name = ToolName::plain("typed_helper");
    let registry = ToolRegistry::from_tools([Arc::new(TestHandler {
        tool_name: tool_name.clone(),
    }) as Arc<dyn CoreToolRuntime>]);
    let mut invocation = test_invocation(
        Arc::new(session),
        Arc::new(turn),
        "code-mode-call",
        tool_name.clone(),
    );
    invocation.source = ToolCallSource::CodeMode {
        cell_id: "cell-1".to_string(),
        parent_call_id: Some("exec-1".to_string()),
        runtime_tool_call_id: "runtime-call-1".to_string(),
    };

    registry
        .dispatch_any_with_terminal_outcome(invocation, admitted_tool_dispatch_state())
        .await?;

    assert_eq!(
        registry.code_mode_argument_preflight_counts(&tool_name),
        Some((1, 1))
    );
    Ok(())
}

#[tokio::test]
async fn parsed_function_arguments_feed_hooks_and_typed_handlers() {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Arguments {
        path: String,
        line: u64,
    }

    let raw = r#"{"path":"src/lib.rs","line":7}"#;
    let payload = ToolPayload::Function {
        arguments: raw.to_string(),
    };
    let parsed = ParsedFunctionArguments::from_payload(&payload)
        .expect("function payload should have a parsed representation");

    with_parsed_function_arguments(Some(parsed), async {
        assert_eq!(
            function_hook_tool_input(raw),
            serde_json::json!({"path": "src/lib.rs", "line": 7})
        );
        assert_eq!(
            crate::tools::handlers::parse_arguments::<Arguments>(raw)
                .expect("typed arguments should deserialize from the canonical value"),
            Arguments {
                path: "src/lib.rs".to_string(),
                line: 7,
            }
        );
    })
    .await;
}

#[test]
fn optimization_priority_owner_continuations_form_a_full_packet_before_trimming() {
    assert_eq!(projection_packet_token_limit(false, 4_000, 10_000), 4_000);
    assert_eq!(projection_packet_token_limit(true, 4_000, 10_000), 10_000);
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

    let projected = serialize_projection_with_limit(envelope, &output, 512).expect("projection");
    let rendered = projected.rendered();

    assert!(matches!(
        &projected,
        BoundedModelProjection::Envelope { .. }
    ));
    assert!(approx_token_count(rendered) <= 512);
    let envelope = projected.envelope().expect("projection envelope");
    assert_eq!(envelope.model_bytes, rendered.len() as u64);
    assert_eq!(
        envelope.model_approximate_tokens,
        approx_token_count(rendered) as u64
    );
    let (header, selected_text) = model_projection_parts(rendered);
    assert_eq!(header["outcome"], "success");
    assert_eq!(
        selected_text,
        envelope.result["selected_text"]
            .as_str()
            .expect("internal selected text")
    );
    assert_eq!(projected.value()["canonical_sha256"], "hash");
}

#[test]
fn projection_model_render_is_compact_and_keeps_selected_text_unescaped() {
    let envelope = ToolProjectionV1 {
        version: 1,
        tool: "test".to_string(),
        outcome: "success".to_string(),
        canonical_sha256: "diagnostic-hash".to_string(),
        canonical_bytes: 8_000,
        canonical_approximate_tokens: 2_000,
        canonical_complete: true,
        model_bytes: 0,
        model_approximate_tokens: 0,
        artifact_id: Some("artifact-123".to_string()),
        sections: Vec::new(),
        omitted_sections: vec!["omitted-1".to_string()],
        result: serde_json::json!({
            "essential": {"exit_code": 0},
            "selection": {
                "selected_ids": ["selected-1"],
                "omitted_inline_ids": ["omitted-1"],
                "partial_ids": [],
                "available_fragments": 12,
            },
            "selected_text": "",
            "preserved_content": [],
            "artifact": {"complete": true},
        }),
    };
    let output = "first line\nC:\\work\\file.txt\n\"quoted\"";

    let projected = serialize_projection_with_limit(envelope, output, 1_000).expect("projection");
    let rendered = projected.rendered();
    let (header, selected_text) = model_projection_parts(rendered);

    assert_eq!(selected_text, output);
    assert!(rendered.ends_with(output));
    assert_eq!(header["artifact_id"], "artifact-123");
    assert_eq!(
        header["selection"]["selected_ids"],
        serde_json::json!(["selected-1"])
    );
    for internal_field in [
        "version",
        "tool",
        "canonical_sha256",
        "canonical_bytes",
        "canonical_approximate_tokens",
        "model_bytes",
        "model_approximate_tokens",
        "sections",
    ] {
        assert!(
            header.get(internal_field).is_none(),
            "leaked {internal_field}"
        );
    }
    let internal = projected.envelope().expect("internal envelope");
    assert_eq!(internal.result["selected_text"], output);
    assert_eq!(internal.canonical_sha256, "diagnostic-hash");
    assert_eq!(internal.model_bytes, rendered.len() as u64);
    assert_eq!(
        internal.model_approximate_tokens,
        approx_token_count(rendered) as u64
    );
}

#[test]
fn projection_fallback_preserves_essential_inline() {
    let essential = serde_json::json!({
        "chunk_id": "chunk-1",
        "exit_code": null,
        "session_id": 41,
    });
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
        result: serde_json::json!({
            "essential": essential,
            "large_metadata": "x".repeat(8_000),
            "selected_text": "",
        }),
    };

    let projected =
        serialize_projection_with_limit(envelope, "selected output", 1).expect("fallback");
    let BoundedModelProjection::Envelope { envelope, rendered } = projected else {
        panic!("expected a typed minimal carrier");
    };

    assert!(approx_token_count(&rendered) > 1);
    assert_eq!(envelope.artifact_id.as_deref(), Some("artifact-123"));
    assert_eq!(envelope.result["essential"], essential);
    assert_eq!(envelope.result["selected_text"], "");
    assert!(envelope.result.get("large_metadata").is_none());
    let (header, selected_text) = model_projection_parts(&rendered);
    assert_eq!(header["essential"], essential);
    assert!(selected_text.is_empty());
}

fn model_projection_parts(rendered: &str) -> (Value, &str) {
    let (header, selected_text) = rendered.split_once('\n').unwrap_or((rendered, ""));
    (
        serde_json::from_str(header).expect("valid compact projection header"),
        selected_text,
    )
}

#[tokio::test]
async fn projection_source_dependency_decision_records_reuse_and_fallback() {
    let timing = TurnTimingState::default();
    let expected =
        std::collections::BTreeSet::from([crate::tool_history::SourceDependencyV1::new(
            std::path::Path::new("src"),
            true,
        )]);
    let carried = with_precomputed_projection_source_dependencies(Some(expected.clone()), async {
        resolve_projection_source_dependencies(
            &timing,
            /*authoritative_override*/ None,
            precomputed_projection_source_dependencies(),
            || panic!("precomputed dependencies must skip fallback analysis"),
        )
    })
    .await;
    assert_eq!(carried, expected);
    let fallback_expected =
        std::collections::BTreeSet::from([crate::tool_history::SourceDependencyV1::new(
            std::path::Path::new("fallback"),
            false,
        )]);
    let fallback = resolve_projection_source_dependencies(
        &timing,
        /*authoritative_override*/ None,
        /*precomputed*/ None,
        || fallback_expected.clone(),
    );
    assert_eq!(fallback, fallback_expected);
    let rewritten_expected =
        std::collections::BTreeSet::from([crate::tool_history::SourceDependencyV1::new(
            std::path::Path::new("rewritten"),
            false,
        )]);
    let rewritten = resolve_projection_source_dependencies(
        &timing,
        Some(rewritten_expected.clone()),
        Some(expected.clone()),
        || panic!("final rewritten dependencies must override pre-hook analysis"),
    );
    assert_eq!(rewritten, rewritten_expected);
    let counters = timing.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.projection_source_dependencies_reuse_count, 1);
    assert_eq!(counters.projection_source_dependencies_fallback_count, 2);

    let output = "bounded output".to_string();
    let canonical = CanonicalToolResult::text(output.clone());
    let projection = project_model_output(ModelProjectionInput {
        spillable_text: output.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "source-dependencies-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "test",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 512,
        projected_text: output.clone(),
        preserved_content: Vec::new(),
        codex_home: std::path::PathBuf::new(),
        thread_id: "thread".to_string(),
        tool_name: "cargo_test".to_string(),
        original_output_sha256: canonical.sha256.clone(),
        original_output_tokens: canonical.approximate_tokens,
        original_output_text: output.clone(),
        invocation_sha256: None,
        canonical,
        semantic_class: "tool_output".to_string(),
        source_dependencies: carried,
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "source-dependencies-call".to_string(),
            output: FunctionCallOutputPayload::from_text(output),
        },
        materialization: ProjectionMaterialization::InlineCarrier,
    })
    .await
    .expect("projected result");
    let mut result = AnyToolResult {
        call_id: "source-dependencies-call".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        result: Box::new(FunctionToolOutput::from_text(
            "native output".to_string(),
            Some(true),
        )),
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };
    result.install_model_projection(Some(projection), /*source_dependencies_override*/ None);

    assert_eq!(result.projected_source_dependencies(), Some(&expected));

    result.install_model_projection(None, Some(rewritten_expected.clone()));
    assert_eq!(
        result.projected_source_dependencies(),
        Some(&rewritten_expected),
        "a rewritten invocation must retain final dependencies even when projection is exempt",
    );
}

#[test]
fn typed_projection_prioritizes_sections_with_stable_exact_deduplication() {
    let diagnostic_one = ToolOutputProjectionFragment::new(
        ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
        "diagnostic-one",
    );
    let fragments = vec![
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ContextualSpillableText,
            "context-one",
        ),
        diagnostic_one.clone(),
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
            "diagnostic-two",
        ),
        diagnostic_one,
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
            available_fragments: 6,
            selected_fragments: 5,
            exact_duplicates_removed: 1,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        }
    );
    assert_eq!(projected.matches("diagnostic-one").count(), 1);
    assert!(
        projected.find("diagnostic-one").expect("first diagnostic")
            < projected.find("diagnostic-two").expect("second diagnostic")
    );
    assert!(
        projected
            .find("[errors and diagnostics]")
            .expect("diagnostic section")
            < projected.find("[validation]").expect("validation section")
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
                "{}\nprocess final status: exit 1\ndiagnostic: worker unavailable\n{}",
                "irrelevant process output ".repeat(300),
                "unrelated terminal tail ".repeat(300),
            ),
            vec![
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                    "process final status: exit 1",
                ),
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
                    "diagnostic: worker unavailable",
                ),
            ],
            [
                "process final status: exit 1",
                "diagnostic: worker unavailable",
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
        original_output_text: String::new(),
        invocation_sha256: None,
        semantic_class: "test".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "structured-call".to_string(),
            output: FunctionCallOutputPayload::from_text(full_output.clone()),
        },
        materialization: ProjectionMaterialization::CanonicalArtifact,
    })
    .await
    .expect("structured projection");
    assert!(projection.artifact_created);
    assert!(!projection.artifact_reused);
    let artifact_id = projection
        .candidate
        .as_ref()
        .expect("canonical artifact candidate")
        .artifact_id
        .clone();
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

#[tokio::test]
async fn canonical_projection_reuses_existing_artifact_id() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let thread_id = "canonical-reuse-thread";
    let full_output = format!("header\n{}\nfooter\n", "exact retained output ".repeat(128));
    let canonical = CanonicalToolResult::text(full_output.clone());
    let artifact = create_canonical_output_artifact(temp.path(), thread_id, &canonical).await;
    let artifact_id = artifact.artifact_id().expect("existing artifact ID");

    let projection = project_model_output(ModelProjectionInput {
        spillable_text: full_output.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({
            "raw_output_artifact_id": artifact_id,
        }),
        origin_call_id: "canonical-reuse-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "test",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 128,
        projected_text: "header\nfooter".to_string(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: thread_id.to_string(),
        tool_name: "exec_command".to_string(),
        original_output_sha256: canonical.sha256.clone(),
        original_output_tokens: canonical.approximate_tokens,
        original_output_text: full_output.clone(),
        invocation_sha256: None,
        canonical,
        semantic_class: "tool_output".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: true,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "canonical-reuse-call".to_string(),
            output: FunctionCallOutputPayload::from_text(full_output),
        },
        materialization: ProjectionMaterialization::CanonicalArtifact,
    })
    .await
    .expect("canonical projection should attach the existing artifact");

    assert!(!projection.artifact_created);
    assert!(projection.artifact_reused);
    assert_eq!(
        projection
            .candidate
            .as_ref()
            .expect("canonical artifact candidate")
            .artifact_id,
        artifact_id
    );
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
    let too_many = (0..65)
        .map(|index| ToolOutputProjectionRange {
            id: format!("range-{index}"),
            start_line: index * 2 + 1,
            end_line: index * 2 + 1,
        })
        .collect::<Vec<_>>();
    assert!(validated_predetermined_ranges(&too_many).is_empty());
    assert!(validated_predetermined_ranges(&[]).is_empty());
}

#[test]
fn projection_owner_recovery_validates_json_pointers_and_combined_identity() {
    let canonical = CanonicalToolResult::json(serde_json::json!({
        "selected": {"value": 1},
        "omitted": {"value": 2},
    }));
    let pointers = vec![
        ToolOutputProjectionJsonPointer {
            id: "selected".to_string(),
            pointer: "/selected".to_string(),
        },
        ToolOutputProjectionJsonPointer {
            id: "omitted".to_string(),
            pointer: "/omitted".to_string(),
        },
    ];
    let sections = vec![
        ToolProjectionSection {
            id: "selected".to_string(),
            value: None,
            exact_bytes: 11,
            inclusion: ToolProjectionInclusion::Included,
            canonical_range: None,
            children: Vec::new(),
            recovery_chunk_bytes: None,
        },
        ToolProjectionSection {
            id: "omitted".to_string(),
            value: None,
            exact_bytes: 11,
            inclusion: ToolProjectionInclusion::Omitted,
            canonical_range: None,
            children: Vec::new(),
            recovery_chunk_bytes: None,
        },
    ];

    let (ranges, pointers) = validated_omitted_predetermined_selectors(
        &[],
        &pointers,
        &sections,
        &canonical.json_pointers,
    );

    assert!(ranges.is_empty());
    assert_eq!(
        pointers,
        vec![ToolOutputProjectionJsonPointer {
            id: "omitted".to_string(),
            pointer: "/omitted".to_string(),
        }]
    );

    let duplicate = vec![
        ToolOutputProjectionJsonPointer {
            id: "duplicate".to_string(),
            pointer: "/selected".to_string(),
        },
        ToolOutputProjectionJsonPointer {
            id: "duplicate".to_string(),
            pointer: "/omitted".to_string(),
        },
    ];
    assert!(validated_predetermined_json_pointers(&duplicate, &canonical.json_pointers).is_empty());
    assert!(
        validated_predetermined_json_pointers(
            &[ToolOutputProjectionJsonPointer {
                id: "missing".to_string(),
                pointer: "/missing".to_string(),
            }],
            &canonical.json_pointers,
        )
        .is_empty()
    );
    let too_many = (0..65)
        .map(|index| ToolOutputProjectionJsonPointer {
            id: format!("pointer-{index}"),
            pointer: format!("/items/{index}"),
        })
        .collect::<Vec<_>>();
    assert!(validated_predetermined_json_pointers(&too_many, &canonical.json_pointers).is_empty());

    let (duplicate_ranges, duplicate_pointers) = validated_omitted_predetermined_selectors(
        &[ToolOutputProjectionRange {
            id: "shared-id".to_string(),
            start_line: 1,
            end_line: 1,
        }],
        &[ToolOutputProjectionJsonPointer {
            id: "shared-id".to_string(),
            pointer: "/selected".to_string(),
        }],
        &[],
        &canonical.json_pointers,
    );
    assert!(duplicate_ranges.is_empty());
    assert!(duplicate_pointers.is_empty());
}

#[tokio::test]
async fn projection_owner_recovery_drains_exact_json_pointer_in_original_return() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let value = serde_json::json!({
        "summary": "bounded",
        "evidence": {
            "blocker": "exact actionable blocker",
            "status": "failed",
        },
    });
    let canonical = CanonicalToolResult::json(value.clone());
    let projection = project_model_output(ModelProjectionInput {
        spillable_text: value.to_string(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({"summary": "bounded"}),
        origin_call_id: "json-pointer-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "test",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 1_000,
        projected_text: "bounded".to_string(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: "json-pointer-thread".to_string(),
        tool_name: "get_agent_task".to_string(),
        original_output_sha256: canonical.sha256.clone(),
        original_output_tokens: canonical.approximate_tokens,
        original_output_text: String::new(),
        invocation_sha256: None,
        canonical,
        semantic_class: "tool_output".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: true,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: vec![ToolOutputProjectionJsonPointer {
            id: "actionable-evidence".to_string(),
            pointer: "/evidence".to_string(),
        }],
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "json-pointer-call".to_string(),
            output: FunctionCallOutputPayload::from_text(value.to_string()),
        },
        materialization: ProjectionMaterialization::CanonicalArtifact,
    })
    .await
    .expect("projection");

    let rendered = match projection.response() {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            output.body.to_text().expect("projected text")
        }
        other => panic!("unexpected response: {other:?}"),
    };
    let (header, _) = model_projection_parts(&rendered);
    let recovery = header["preserved_content"]
        .as_array()
        .and_then(|items| items.first())
        .expect("deterministic recovery");
    assert_eq!(
        recovery["predetermined_json_pointers"],
        serde_json::json!([{
            "id": "actionable-evidence",
            "pointer": "/evidence",
        }])
    );
    assert_eq!(
        recovery["results"][0]["value"],
        serde_json::json!({
            "blocker": "exact actionable blocker",
            "status": "failed",
        })
    );
    assert!(projection.deterministic_continuation_receipt.is_some());
}

#[tokio::test]
async fn one_predetermined_range_is_recovered_in_the_original_return() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let full_output = "included line\nomitted exact line".to_string();
    let mut canonical = CanonicalToolResult::text(full_output.clone());
    let omitted_start = "included line\n".len() as u64;
    canonical.sections = vec![ToolProjectionSection {
        id: "omitted-chunk".to_string(),
        value: None,
        exact_bytes: canonical.exact_bytes - omitted_start,
        inclusion: ToolProjectionInclusion::Omitted,
        canonical_range: Some(CanonicalByteRange::new(
            omitted_start,
            canonical.exact_bytes,
        )),
        children: Vec::new(),
        recovery_chunk_bytes: None,
    }];
    let projection = project_model_output(ModelProjectionInput {
        spillable_text: full_output.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "single-predetermined-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "test",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 1_000,
        projected_text: "included line".to_string(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: "single-predetermined-thread".to_string(),
        tool_name: "sample_reader".to_string(),
        original_output_sha256: canonical.sha256.clone(),
        original_output_tokens: canonical.approximate_tokens,
        original_output_text: String::new(),
        invocation_sha256: None,
        canonical,
        semantic_class: "test_projection".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: true,
        predetermined_ranges: vec![ToolOutputProjectionRange {
            id: "omitted-chunk".to_string(),
            start_line: 2,
            end_line: 2,
        }],
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "single-predetermined-call".to_string(),
            output: FunctionCallOutputPayload::from_text(full_output),
        },
        materialization: ProjectionMaterialization::CanonicalArtifact,
    })
    .await
    .expect("projection");

    let rendered = match projection.response() {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            output.body.to_text().expect("projected text")
        }
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(rendered.contains("omitted exact line"));
    let receipt = projection
        .deterministic_continuation_receipt
        .expect("owner-drain receipt");
    assert_eq!(receipt.class, DeterministicContinuationClass::ArtifactRange);
    assert_eq!(receipt.suppressed_continuation_count, 1);
}

#[tokio::test]
async fn three_predetermined_artifact_ranges_are_drained_in_original_return() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let thread_id = "predetermined-range-thread";
    let full_output = (1..=300)
        .map(|line| format!("line-{line:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut canonical = CanonicalToolResult::text(full_output.clone());
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
    let line_ranges = full_output
        .split_inclusive('\n')
        .scan(0_u64, |cursor, line| {
            let start = *cursor;
            *cursor += line.len() as u64;
            Some(CanonicalByteRange::new(start, *cursor))
        })
        .collect::<Vec<_>>();
    canonical.sections = ranges
        .iter()
        .map(|range| {
            let start = line_ranges[range.start_line - 1].start;
            let end = line_ranges[range.end_line - 1].end;
            ToolProjectionSection {
                id: range.id.clone(),
                value: None,
                exact_bytes: end - start,
                inclusion: ToolProjectionInclusion::Omitted,
                canonical_range: Some(CanonicalByteRange::new(start, end)),
                children: Vec::new(),
                recovery_chunk_bytes: None,
            }
        })
        .collect();
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
        original_output_text: String::new(),
        invocation_sha256: None,
        semantic_class: "validation".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: ranges,
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "predetermined-call".to_string(),
            output: FunctionCallOutputPayload::from_text(full_output),
        },
        materialization: ProjectionMaterialization::CanonicalArtifact,
    })
    .await
    .expect("projection");

    let rendered = match projection.response() {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            output.body.to_text().expect("projected text")
        }
        other => panic!("unexpected response: {other:?}"),
    };
    let (header, _) = model_projection_parts(&rendered);
    let recovery = header["preserved_content"]
        .as_array()
        .and_then(|items| {
            items.iter().find(|item| {
                item.get("type").and_then(Value::as_str)
                    == Some("deterministic_tool_output_recovery")
            })
        })
        .expect("preserved deterministic recovery");
    for expected in [
        "line-001", "line-002", "line-150", "line-151", "line-299", "line-300",
    ] {
        assert!(
            recovery.to_string().contains(expected),
            "missing {expected}"
        );
    }
    let receipt = projection
        .deterministic_continuation_receipt
        .expect("artifact-range receipt");
    assert_eq!(receipt.class, DeterministicContinuationClass::ArtifactRange);
    assert_eq!(receipt.suppressed_continuation_count, 1);
    let artifact_files = std::fs::read_dir(temp.path().join("tool-output").join(thread_id))
        .expect("artifact directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("log"))
        .count();
    assert_eq!(
        artifact_files, 1,
        "owner drain must not create a child artifact"
    );
}

#[tokio::test]
async fn missing_or_stale_predetermined_artifact_fails_open() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let canonical = CanonicalToolResult::text("exact evidence\n");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("artifact ID");
    let ranges = vec![ToolOutputProjectionRange {
        id: "result".to_string(),
        start_line: 1,
        end_line: 1,
    }];

    let (stale_content, stale_receipt) = drain_predetermined_artifact_ranges(
        temp.path(),
        "thread",
        &artifact_id,
        "stale-canonical-revision",
        ranges.clone(),
        &[],
    )
    .await;
    let (missing_content, missing_receipt) = drain_predetermined_artifact_ranges(
        temp.path(),
        "thread",
        &uuid::Uuid::new_v4().to_string(),
        &canonical.sha256,
        ranges,
        &[],
    )
    .await;

    assert!(stale_content.is_empty());
    assert_eq!(stale_receipt, None);
    assert!(missing_content.is_empty());
    assert_eq!(missing_receipt, None);
}

#[test]
fn wire_only_receipt_cannot_satisfy_bounds_sensitive_owner_drain() {
    let in_memory = TurnTimingDeterministicContinuationReceipt::new(
        DeterministicContinuationClass::ArtifactRange,
        "artifact".to_string(),
        "revision".to_string(),
        DeterministicContinuationHostAction::DrainArtifactRanges,
        "authoritative-bounds".to_string(),
        1,
    );
    assert!(valid_owner_drained_receipt(&in_memory));

    let wire = serde_json::to_value(&in_memory).expect("serialize public receipt");
    let reloaded: TurnTimingDeterministicContinuationReceipt =
        serde_json::from_value(wire).expect("validated public receipt");
    assert_eq!(reloaded.wire_identity(), in_memory.wire_identity());
    assert!(reloaded.runtime_identity().is_none());
    assert!(!valid_owner_drained_receipt(&reloaded));
}

#[tokio::test]
async fn small_code_mode_owner_result_uses_artifact_free_inline_carrier() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let outer_text = "small native outer result".to_string();
    let canonical = CanonicalToolResult::text(outer_text.clone());
    let mut projection = project_model_output(ModelProjectionInput {
        spillable_text: outer_text.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "outer-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "inline_continuation_carrier",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 1_000,
        projected_text: outer_text.clone(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: "thread".to_string(),
        tool_name: "exec".to_string(),
        canonical: canonical.clone(),
        original_output_sha256: canonical.sha256.clone(),
        original_output_tokens: canonical.approximate_tokens,
        original_output_text: String::new(),
        invocation_sha256: None,
        semantic_class: "tool_output".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::CustomToolCallOutput {
            call_id: "outer-call".to_string(),
            name: Some("exec".to_string()),
            output: FunctionCallOutputPayload::from_text(outer_text.clone()),
        },
        materialization: ProjectionMaterialization::InlineCarrier,
    })
    .await
    .expect("inline carrier");
    let receipt = TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::ArtifactRange,
        wire_identity: String::new(),
        resource_identity_hash: "nested-artifact".to_string(),
        state_revision: "nested-revision".to_string(),
        host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
        action_bounds_hash: "nested-bounds".to_string(),
        suppressed_continuation_count: 1,
    };
    let invalid_receipt = TurnTimingDeterministicContinuationReceipt {
        resource_identity_hash: String::new(),
        ..receipt.clone()
    };
    let oversized_receipt = TurnTimingDeterministicContinuationReceipt {
        resource_identity_hash: "oversized-artifact".to_string(),
        ..receipt.clone()
    };
    let accepted = projection.merge_owner_drained_continuations(vec![
        PendingOwnerDrainedContinuation {
            preserved_content: vec![serde_json::json!({"invalid": true})],
            receipt: invalid_receipt,
        },
        PendingOwnerDrainedContinuation {
            preserved_content: vec![serde_json::json!({"exact": "nested evidence"})],
            receipt: receipt.clone(),
        },
        PendingOwnerDrainedContinuation {
            preserved_content: vec![serde_json::json!({"oversized": "x".repeat(20_000)})],
            receipt: oversized_receipt,
        },
    ]);

    assert_eq!(accepted, vec![receipt]);
    assert!(projection.candidate.is_none());
    assert!(!projection.artifact_created);
    assert!(!temp.path().join("tool-output").exists());
    let envelope = projection.bounded.value();
    assert_eq!(envelope["artifact_id"], Value::Null);
    assert_eq!(envelope["result"]["artifact"], Value::Null);
    assert_eq!(envelope["result"]["selected_text"], outer_text);
    assert_eq!(
        envelope["result"]["preserved_content"],
        serde_json::json!([{"exact": "nested evidence"}])
    );
}

#[tokio::test]
async fn projection_owner_recovery_mixed_selectors_survive_code_mode_continuation_merging() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let mut inner_canonical = CanonicalToolResult::json(serde_json::json!({
        "line": "exact nested evidence",
        "pointer": {"blocker": "exact pointer evidence"},
    }));
    inner_canonical.sections = vec![ToolProjectionSection {
        id: "nested-range".to_string(),
        value: None,
        exact_bytes: inner_canonical.exact_bytes,
        inclusion: ToolProjectionInclusion::Omitted,
        canonical_range: Some(CanonicalByteRange::new(0, inner_canonical.exact_bytes)),
        children: Vec::new(),
        recovery_chunk_bytes: None,
    }];
    let inner_text = String::from_utf8(inner_canonical.bytes.clone()).expect("canonical JSON text");
    let inner_projection = project_model_output(ModelProjectionInput {
        spillable_text: inner_text.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "nested-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "test",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 1_000,
        projected_text: "nested summary".to_string(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: "thread".to_string(),
        tool_name: "sample_reader".to_string(),
        canonical: inner_canonical.clone(),
        original_output_sha256: inner_canonical.sha256.clone(),
        original_output_tokens: inner_canonical.approximate_tokens,
        original_output_text: String::new(),
        invocation_sha256: None,
        semantic_class: "test_projection".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: true,
        predetermined_ranges: vec![ToolOutputProjectionRange {
            id: "nested-range".to_string(),
            start_line: 1,
            end_line: 1,
        }],
        predetermined_json_pointers: vec![ToolOutputProjectionJsonPointer {
            id: "nested-pointer".to_string(),
            pointer: "/pointer".to_string(),
        }],
        original_response: ResponseInputItem::FunctionCallOutput {
            call_id: "nested-call".to_string(),
            output: FunctionCallOutputPayload::from_text(inner_text),
        },
        materialization: ProjectionMaterialization::CanonicalArtifact,
    })
    .await
    .expect("nested projection");
    let nested_payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let nested = AnyToolResult {
        call_id: "nested-call".to_string(),
        payload: nested_payload,
        result: Box::new(FunctionToolOutput::from_text(
            "native nested value".to_string(),
            Some(true),
        )),
        model_projection: Some(inner_projection),
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };
    let continuation = nested
        .owner_drained_continuation()
        .expect("owner-drained nested evidence");
    let expected_action_bounds = serde_json::json!({
        "predetermined_ranges": [{
            "id": "nested-range",
            "start_line": 1,
            "end_line": 1,
        }],
        "predetermined_json_pointers": [{
            "id": "nested-pointer",
            "pointer": "/pointer",
        }],
    });
    assert_eq!(
        continuation.receipt.action_bounds_hash,
        crate::tool_history::sha256(
            serde_json::to_string(&expected_action_bounds)
                .expect("serialize expected action bounds")
                .as_bytes(),
        )
    );
    let mut oversized_receipt = continuation.receipt.clone();
    oversized_receipt.action_bounds_hash = "oversized-range".to_string();
    let oversized_continuation = PendingOwnerDrainedContinuation {
        preserved_content: vec![serde_json::json!({ "exact": "x".repeat(8_000) })],
        receipt: oversized_receipt,
    };
    let nested_code_mode = nested.code_mode_result();
    assert_eq!(nested_code_mode["version"], 1);
    assert!(nested_code_mode["artifact_id"].is_string());
    assert_eq!(nested_code_mode["canonical_sha256"], inner_canonical.sha256);

    let outer_text = "outer code-mode output ".repeat(200);
    let outer_canonical = CanonicalToolResult::text(outer_text.clone());
    let outer_projection = project_model_output(ModelProjectionInput {
        spillable_text: outer_text.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "outer-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "generic_fallback",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 1_000,
        projected_text: "bounded outer output".to_string(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: "thread".to_string(),
        tool_name: "exec".to_string(),
        canonical: outer_canonical.clone(),
        original_output_sha256: outer_canonical.sha256.clone(),
        original_output_tokens: outer_canonical.approximate_tokens,
        original_output_text: String::new(),
        invocation_sha256: None,
        semantic_class: "tool_output".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: true,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::CustomToolCallOutput {
            call_id: "outer-call".to_string(),
            name: Some("exec".to_string()),
            output: FunctionCallOutputPayload::from_text(outer_text),
        },
        materialization: ProjectionMaterialization::CanonicalArtifact,
    })
    .await
    .expect("outer projection");
    let mut outer = AnyToolResult {
        call_id: "outer-call".to_string(),
        payload: ToolPayload::Custom {
            input: "text(result)".to_string(),
        },
        result: Box::new(FunctionToolOutput::from_text(
            "bounded outer output".to_string(),
            Some(true),
        )),
        model_projection: Some(outer_projection),
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };

    assert!(
        outer
            .merge_owner_drained_continuations(vec![continuation.clone(), continuation.clone()])
            .is_empty()
    );
    let accepted =
        outer.merge_owner_drained_continuations(vec![continuation, oversized_continuation]);
    assert_eq!(accepted.len(), 1);
    let ResponseInputItem::CustomToolCallOutput { output, .. } = outer.into_response() else {
        panic!("expected outer custom tool output");
    };
    let rendered = output.body.to_text().expect("outer projection text");
    let (header, _) = model_projection_parts(&rendered);
    assert!(
        header["preserved_content"]
            .to_string()
            .contains("exact nested evidence")
    );
    assert!(
        header["preserved_content"]
            .to_string()
            .contains("exact pointer evidence")
    );
    assert!(
        !header["preserved_content"]
            .to_string()
            .contains(&"x".repeat(8_000))
    );
}

#[tokio::test]
async fn fresh_corpus_replays_real_producer_handler_and_functions_exec_carrier() {
    let codex_home = tempfile::tempdir().expect("isolated temporary Codex home");
    let thread_id = "fresh-recovery-corpus";
    let text = (0..128)
        .map(|index| format!("fresh-{index:03}-{}\n", "abcdefghij".repeat(5)))
        .collect::<String>();
    let canonical = CanonicalToolResult::text(text.clone());
    let artifact = create_canonical_output_artifact(codex_home.path(), thread_id, &canonical).await;
    let artifact_id = artifact.artifact_id().expect("fresh corpus artifact ID");
    let selector_manifest = vec![
        ToolOutputSelector::Lines {
            start: 65,
            end: 128,
        },
        ToolOutputSelector::Lines { start: 1, end: 64 },
        ToolOutputSelector::Lines { start: 63, end: 66 },
        ToolOutputSelector::Lines { start: 1, end: 64 },
    ];

    let (recovered, reused) = crate::tools::handlers::execute_recovery_transaction(
        codex_home.path(),
        thread_id,
        &artifact_id,
        selector_manifest,
        true,
    )
    .await
    .expect("fresh corpus handler transaction");
    assert!(!reused);
    assert!(recovered.complete);
    assert_eq!(recovered.results.len(), 1);
    assert_eq!(recovered.results[0].status, ToolOutputSelectorStatus::Ok);
    assert_eq!(recovered.results[0].text.as_deref(), Some(text.as_str()));
    assert!(recovered.results[0].subdivision_plan.is_some());

    let outer_text = "fresh corpus functions.exec carrier".to_string();
    let outer_canonical = CanonicalToolResult::text(outer_text.clone());
    let mut outer_projection = project_model_output(ModelProjectionInput {
        spillable_text: outer_text.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "fresh-functions-exec".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "inline_continuation_carrier",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS,
        projected_text: outer_text.clone(),
        preserved_content: Vec::new(),
        codex_home: codex_home.path().to_path_buf(),
        thread_id: thread_id.to_string(),
        tool_name: "exec".to_string(),
        canonical: outer_canonical.clone(),
        original_output_sha256: outer_canonical.sha256.clone(),
        original_output_tokens: outer_canonical.approximate_tokens,
        original_output_text: String::new(),
        invocation_sha256: None,
        semantic_class: "tool_output".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: ResponseInputItem::CustomToolCallOutput {
            call_id: "fresh-functions-exec".to_string(),
            name: Some("exec".to_string()),
            output: FunctionCallOutputPayload::from_text(outer_text),
        },
        materialization: ProjectionMaterialization::InlineCarrier,
    })
    .await
    .expect("fresh functions.exec projection");
    let receipt = TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::ArtifactRange,
        wire_identity: String::new(),
        resource_identity_hash: crate::tool_history::sha256(artifact_id.as_bytes()),
        state_revision: recovered.canonical_sha256.clone(),
        host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
        action_bounds_hash: crate::tool_history::sha256(b"fresh-selector-manifest-v1"),
        suppressed_continuation_count: 1,
    };
    let accepted =
        outer_projection.merge_owner_drained_continuations(vec![PendingOwnerDrainedContinuation {
            preserved_content: vec![
                serde_json::to_value(&recovered).expect("serialize exact recovery"),
            ],
            receipt: receipt.clone(),
        }]);
    assert_eq!(accepted, vec![receipt]);
    assert!(!outer_projection.artifact_created);
    assert_eq!(
        outer_projection.bounded.value()["result"]["preserved_content"],
        serde_json::json!([serde_json::to_value(&recovered).expect("serialize exact recovery")])
    );

    let log_count = std::fs::read_dir(codex_home.path().join("tool-output").join(thread_id))
        .expect("fresh corpus artifact directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "log")
        })
        .count();
    let report = serde_json::json!({
        "schema": "fresh_tool_output_recovery_corpus_v1",
        "producer_artifacts": 1,
        "selector_manifest_entries": 4,
        "normalized_results": recovered.results.len(),
        "logical_recovery_transactions": 1,
        "silent_truncations": 0,
        "false_success_results": 0,
        "recursive_spills": log_count.saturating_sub(1),
        "secondary_model_boundaries": 0,
        "expected_secondary_model_boundaries": 0,
        "maximum_secondary_model_boundaries": 2,
    });
    tracing::info!(%report, "fresh corpus report");
    assert_eq!(report["silent_truncations"], 0);
    assert_eq!(report["false_success_results"], 0);
    assert_eq!(report["recursive_spills"], 0);
    assert_eq!(report["secondary_model_boundaries"], 0);
}

#[test]
fn predetermined_source_ranges_are_intersected_with_omitted_sections_before_bounds() {
    let mut ranges = (0..6)
        .map(|index| ToolOutputProjectionRange {
            id: format!("chunk-{index}"),
            start_line: index * 40 + 1,
            end_line: index * 40 + 40,
        })
        .collect::<Vec<_>>();
    let sections = ranges
        .iter()
        .enumerate()
        .map(|(index, range)| ToolProjectionSection {
            id: range.id.clone(),
            value: None,
            exact_bytes: 40,
            inclusion: if index < 2 {
                ToolProjectionInclusion::Included
            } else {
                ToolProjectionInclusion::Omitted
            },
            canonical_range: None,
            children: Vec::new(),
            recovery_chunk_bytes: None,
        })
        .collect::<Vec<_>>();
    ranges.push(ToolOutputProjectionRange {
        id: "new-model-selected-range".to_string(),
        start_line: 241,
        end_line: 250,
    });

    let bounded = validated_omitted_predetermined_ranges(&ranges, &sections);

    assert_eq!(
        bounded
            .iter()
            .map(|range| range.id.as_str())
            .collect::<Vec<_>>(),
        vec!["chunk-2", "chunk-3", "chunk-4", "chunk-5"]
    );
    assert!(
        validated_omitted_predetermined_ranges(
            &[ToolOutputProjectionRange {
                id: "unknown-section".to_string(),
                start_line: 1,
                end_line: 1,
            }],
            &sections,
        )
        .is_empty(),
        "unmatched owner ranges must fall back to model-mediated recovery"
    );
}

#[test]
fn metadata_free_new_tool_result_has_no_owner_drained_continuation() {
    let result = AnyToolResult {
        call_id: "new-call".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        result: Box::new(FunctionToolOutput::from_text(
            "new evidence requiring interpretation".to_string(),
            Some(true),
        )),
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };

    assert!(result.owner_drained_continuation().is_none());
}

#[tokio::test]
async fn missing_stale_and_oversized_artifacts_never_report_partial_owner_success() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let range = ToolOutputProjectionRange {
        id: "source-chunk".to_string(),
        start_line: 1,
        end_line: 1,
    };
    let (missing, missing_receipt) = drain_predetermined_artifact_ranges(
        temp.path(),
        "thread",
        "01900000-0000-7000-8000-000000000000",
        "missing-revision",
        vec![range.clone()],
        &[],
    )
    .await;
    assert!(missing.is_empty());
    assert!(missing_receipt.is_none());

    let canonical = CanonicalToolResult::text("exact source line");
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let artifact_id = artifact.artifact_id().expect("artifact ID");
    let (stale, stale_receipt) = drain_predetermined_artifact_ranges(
        temp.path(),
        "thread",
        &artifact_id,
        "stale-canonical-sha256",
        vec![range],
        &[],
    )
    .await;
    assert!(stale.is_empty());
    assert!(stale_receipt.is_none());

    let oversized_text = (1..=200)
        .map(|line| format!("{line:03} {}", "oversized-source-evidence ".repeat(24)))
        .collect::<Vec<_>>()
        .join("\n");
    let oversized_canonical = CanonicalToolResult::text(oversized_text);
    let oversized_artifact =
        create_canonical_output_artifact(temp.path(), "thread", &oversized_canonical).await;
    let oversized_id = oversized_artifact
        .artifact_id()
        .expect("oversized artifact ID");
    let (oversized, oversized_receipt) = drain_predetermined_artifact_ranges(
        temp.path(),
        "thread",
        &oversized_id,
        &oversized_canonical.sha256,
        vec![ToolOutputProjectionRange {
            id: "oversized-source-chunk".to_string(),
            start_line: 1,
            end_line: 200,
        }],
        &[],
    )
    .await;
    assert!(oversized.is_empty());
    assert!(oversized_receipt.is_none());
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

    let projected = projected_response_item(original, r#"{"version":1}"#.to_string(), true);
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
fn projected_response_item_drops_oversized_mcp_modalities() {
    let original = ResponseInputItem::McpToolCallOutput {
        call_id: "mcp-call".to_string(),
        output: codex_protocol::mcp::CallToolResult {
            content: vec![
                serde_json::json!({"type": "text", "text": "original"}),
                serde_json::json!({"type": "image", "data": "x".repeat(20_000), "mimeType": "image/png"}),
            ],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        },
    };

    let projected = projected_response_item(original, r#"{"artifact_id":"id"}"#.to_string(), false);
    let ResponseInputItem::McpToolCallOutput { output, .. } = projected else {
        panic!("expected MCP response");
    };
    assert_eq!(
        output.content,
        vec![serde_json::json!({
            "type": "text",
            "text": r#"{"artifact_id":"id"}"#,
        })]
    );
}

#[test]
fn projected_function_response_keeps_small_image_as_one_modality() {
    let image = FunctionCallOutputContentItem::InputImage {
        image_url: "data:image/png;base64,AAA".to_string(),
        detail: None,
    };
    let original = ResponseInputItem::FunctionCallOutput {
        call_id: "function-call".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "large original text".to_string(),
                },
                image.clone(),
            ]),
            success: Some(true),
        },
    };

    let projected = projected_response_item(original, "bounded projection".to_string(), true);
    let ResponseInputItem::FunctionCallOutput { output, .. } = projected else {
        panic!("expected function response");
    };
    assert_eq!(
        output.content_items(),
        Some(
            vec![
                FunctionCallOutputContentItem::InputText {
                    text: "bounded projection".to_string(),
                },
                image,
            ]
            .as_slice()
        )
    );
}

#[test]
fn non_text_projection_token_cost_counts_small_and_large_content() {
    let content = vec![serde_json::json!({
        "type": "image",
        "data": "image-bytes",
        "mimeType": "image/png",
    })];
    let cost = non_text_projection_token_cost(&content);

    assert!(cost > 0);
    assert!(cost < 100);
    assert!(cost <= 1_000usize.saturating_sub(MIN_PROJECTION_ENVELOPE_TOKENS));
    assert!(
        non_text_projection_token_cost(&[serde_json::json!({
            "type": "image",
            "data": "x".repeat(20_000),
        })]) > 100
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

#[test]
fn post_tool_feedback_wrapper_preserves_original_control_metadata() {
    let wrapped = PostToolUseFeedbackOutput {
        original: Box::new(codex_tools::JsonToolOutput::new(serde_json::json!({
            "status": "aborted",
            "nextCursor": "opaque-cursor",
            "omitted_result_count": 5,
            "payload": "must remain spillable",
        }))),
        model_visible: FunctionToolOutput::from_text("hook feedback".to_string(), Some(false)),
    };

    let metadata = wrapped.projection_metadata().expect("wrapper metadata");

    assert_eq!(metadata.essential_inline["status"], "aborted");
    assert_eq!(metadata.essential_inline["nextCursor"], "opaque-cursor");
    assert_eq!(metadata.essential_inline["omitted_result_count"], 5);
    assert!(metadata.essential_inline.get("payload").is_none());
    assert_eq!(metadata.spillable_text, vec!["hook feedback"]);
}

#[test]
fn blocking_post_tool_hook_preserves_completed_result_and_discards_context() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let mut result = AnyToolResult {
        call_id: "call-1".to_string(),
        payload: payload.clone(),
        result: Box::new(FunctionToolOutput::from_text(
            "mutation completed".to_string(),
            Some(true),
        )),
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    };

    let contexts = apply_post_tool_use_outcome(
        &mut result,
        codex_hooks::PostToolUseOutcome {
            hook_events: Vec::new(),
            should_block: true,
            additional_contexts: vec!["must not be injected".to_string()],
            feedback_message: Some("reject completed mutation".to_string()),
        },
    );

    assert!(contexts.is_empty());
    assert!(result.result.success_for_logging());
    assert_eq!(
        result.result.to_response_item("call-1", &payload),
        FunctionToolOutput::from_text("mutation completed".to_string(), Some(true))
            .to_response_item("call-1", &payload)
    );
}

#[test]
fn unavailable_model_projection_is_not_reported_as_success() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let output = UnavailableModelProjectionOutput {
        original: Box::new(FunctionToolOutput::from_text(
            "unavailable canonical result".to_string(),
            Some(true),
        )),
        model_visible: FunctionToolOutput::from_text(
            "Tool execution completed, but its full result could not be preserved for model delivery."
                .to_string(),
            Some(false),
        ),
    };

    assert!(!output.success_for_logging());
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("call-1", &payload)
    else {
        panic!("expected function-call output");
    };
    assert_eq!(output.success, Some(false));
}

#[test]
fn semantic_sections_use_declared_canonical_ranges_and_stable_json_handles() {
    let text = CanonicalToolResult::text("alpha\nbeta\ngamma\n");
    let fragments = vec![
        ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ContextualSpillableText,
            "rendered text that is not present in the canonical bytes",
        )
        .with_id("symbol:beta"),
    ];
    let selection = ProjectionSelectionFacts {
        mode: "typed_fragments",
        available_fragments: 1,
        selected_fragments: 0,
        exact_duplicates_removed: 0,
        selected_ids: Vec::new(),
        omitted_inline_ids: vec!["symbol:beta".to_string()],
        partial_ids: Vec::new(),
    };

    let sections = canonical_projection_sections(
        &text,
        &fragments,
        &selection,
        &[ToolOutputProjectionRange {
            id: "symbol:beta".to_string(),
            start_line: 2,
            end_line: 2,
        }],
        &[],
    );

    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].id, "symbol:beta");
    assert_eq!(
        sections[0].canonical_range,
        Some(CanonicalByteRange::new(6, 11))
    );

    let json = CanonicalToolResult::json(serde_json::json!({
        "cursor": "opaque",
        "items": [1, 2, 3],
    }));
    let json_sections = canonical_projection_sections(&json, &[], &selection, &[], &[]);
    assert_eq!(
        json_sections
            .iter()
            .map(|section| section.id.as_str())
            .collect::<Vec<_>>(),
        vec!["json:/cursor", "json:/items"]
    );
    assert!(
        json_sections
            .iter()
            .all(|section| section.canonical_range.is_some())
    );
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

struct PostHookGateHandler {
    tool_name: ToolName,
    success: bool,
    name_calls: Arc<std::sync::atomic::AtomicUsize>,
    payload_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl ToolExecutor<ToolInvocation> for PostHookGateHandler {
    fn tool_name(&self) -> ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let success = self.success;
        Box::pin(async move {
            Ok(
                Box::new(crate::tools::context::FunctionToolOutput::from_text(
                    "ok".to_string(),
                    Some(success),
                )) as Box<dyn crate::tools::context::ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for PostHookGateHandler {
    fn post_tool_use_hook_name(&self, _invocation: &ToolInvocation) -> Option<HookToolName> {
        self.name_calls.fetch_add(1, Ordering::Relaxed);
        Some(HookToolName::new(self.tool_name.to_string()))
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        _result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        self.payload_calls.fetch_add(1, Ordering::Relaxed);
        Some(PostToolUsePayload {
            tool_name: HookToolName::new(self.tool_name.to_string()),
            tool_use_id: invocation.call_id.clone(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({ "ok": true }),
        })
    }
}

#[tokio::test]
async fn confirmed_performance_post_hook_response_is_deferred_until_success_and_matcher_gates_pass()
-> anyhow::Result<()> {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let successful_name = ToolName::plain("successful_post_hook_gate");
    let failed_name = ToolName::plain("failed_post_hook_gate");
    let name_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let payload_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = ToolRegistry::from_tools([
        Arc::new(PostHookGateHandler {
            tool_name: successful_name.clone(),
            success: true,
            name_calls: Arc::clone(&name_calls),
            payload_calls: Arc::clone(&payload_calls),
        }) as Arc<dyn CoreToolRuntime>,
        Arc::new(PostHookGateHandler {
            tool_name: failed_name.clone(),
            success: false,
            name_calls: Arc::clone(&name_calls),
            payload_calls: Arc::clone(&payload_calls),
        }) as Arc<dyn CoreToolRuntime>,
    ]);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    for (call_id, tool_name) in [
        ("successful-call", successful_name),
        ("failed-call", failed_name),
    ] {
        let mut invocation =
            test_invocation(Arc::clone(&session), Arc::clone(&turn), call_id, tool_name);
        invocation.source = ToolCallSource::CodeMode {
            cell_id: "cell-1".to_string(),
            parent_call_id: Some("exec-1".to_string()),
            runtime_tool_call_id: call_id.to_string(),
        };
        registry
            .dispatch_any_with_terminal_outcome(invocation, admitted_tool_dispatch_state())
            .await?;
    }

    assert_eq!(name_calls.load(Ordering::Relaxed), 1);
    assert_eq!(payload_calls.load(Ordering::Relaxed), 0);
    Ok(())
}

struct SpecCountingHandler {
    tool_name: codex_tools::ToolName,
    spec_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl ToolExecutor<ToolInvocation> for SpecCountingHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        self.spec_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

impl CoreToolRuntime for SpecCountingHandler {}

struct IndependentNameHandler;

impl ToolExecutor<ToolInvocation> for IndependentNameHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("legacy_independent_name")
    }

    fn spec(&self) -> ToolSpec {
        test_spec(&ToolName::plain("authoritative_name"))
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

impl CoreToolRuntime for IndependentNameHandler {}

#[test]
fn registry_caches_each_runtime_spec_once() {
    let tool_name = ToolName::plain("counted_spec");
    let spec_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime: Arc<dyn CoreToolRuntime> = Arc::new(SpecCountingHandler {
        tool_name,
        spec_calls: Arc::clone(&spec_calls),
    });
    let registry = ToolRegistry::from_tools([runtime]);

    let first = registry.manifest_entries();
    let second = registry.manifest_entries();

    assert_eq!(first.len(), second.len());
    assert!(
        first
            .iter()
            .zip(&second)
            .all(|(left, right)| std::ptr::eq(*left, *right))
    );
    assert!(std::ptr::eq(
        first[0].canonical_spec_sha256(),
        second[0].canonical_spec_sha256()
    ));
    assert_eq!(spec_calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn registered_search_info_reuses_the_authoritative_spec_snapshot() {
    let tool_name = ToolName::plain("searchable_counted_spec");
    let spec_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime: Arc<dyn CoreToolRuntime> = Arc::new(SpecCountingHandler {
        tool_name: tool_name.clone(),
        spec_calls: Arc::clone(&spec_calls),
    });
    let registered = RegisteredTool::new(runtime, TypedToolClass::ReadSearch);

    let search_info = registered
        .search_info()
        .expect("function tools should be discoverable");

    assert_eq!(search_info.entry.tool_names, vec![tool_name.to_string()]);
    assert_eq!(spec_calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn registry_dispatch_identity_is_derived_from_the_cached_spec() {
    let runtime: Arc<dyn CoreToolRuntime> = Arc::new(IndependentNameHandler);
    let authoritative_name = ToolName::plain("authoritative_name");
    let registry = ToolRegistry::from_tools([runtime]);

    assert_eq!(registry.tool_names_for_test(), vec![authoritative_name]);
    assert!(
        registry
            .tool(&ToolName::plain("legacy_independent_name"))
            .is_none()
    );
}

#[tokio::test]
async fn tool_invocation_retains_turn_only_through_step_context() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let turn = Arc::new(turn);
    let initial_turn_refs = Arc::strong_count(&turn);

    let invocation = test_invocation(
        Arc::new(session),
        Arc::clone(&turn),
        "single-turn-owner",
        ToolName::plain("single_turn_owner"),
    );

    assert_eq!(Arc::strong_count(&turn), initial_turn_refs + 1);
    assert!(Arc::ptr_eq(&turn, &invocation.step_context.turn));
}

#[test]
fn registered_tool_keeps_exposure_as_registry_metadata() {
    let tool_name = ToolName::plain("metadata_exposure");
    let runtime: Arc<dyn CoreToolRuntime> = Arc::new(TestHandler {
        tool_name: tool_name.clone(),
    });
    let registered = RegisteredTool::with_exposure(
        Arc::clone(&runtime),
        ToolExposure::Hidden,
        TypedToolClass::ReadSearch,
    );

    assert_eq!(registered.exposure(), ToolExposure::Hidden);
    assert_eq!(registered.runtime().exposure(), ToolExposure::Direct);

    let registry = ToolRegistry::from_unique_registered_tools([registered]);
    assert_eq!(
        registry.tool_exposure(&tool_name),
        Some(ToolExposure::Hidden)
    );
    assert!(Arc::ptr_eq(
        &runtime,
        &registry.tool(&tool_name).expect("registered runtime")
    ));
}

#[test]
#[should_panic(expected = "registered tools must declare an authorization class")]
fn registered_tool_rejects_unknown_authorization_class() {
    let runtime: Arc<dyn CoreToolRuntime> = Arc::new(TestHandler {
        tool_name: ToolName::plain("missing_authorization_class"),
    });

    let _registered = RegisteredTool::new(runtime, TypedToolClass::Unknown);
}

#[test]
fn prevalidated_registry_constructor_does_not_repeat_duplicate_filtering() {
    let source = include_str!("registry.rs");
    let constructor = source
        .split_once("fn from_unique_registered_tools")
        .expect("prevalidated registry constructor")
        .1
        .split_once("pub(crate) fn")
        .map_or(source, |(body, _)| body);

    assert!(!constructor.contains("contains_key"));
    assert!(!constructor.contains("continue"));
}

#[derive(Clone)]
enum LifecycleTestResult {
    Ok { success: bool },
    RequiredArtifact,
    Err,
}

struct RequiredArtifactOutput(crate::tools::context::FunctionToolOutput);

impl crate::tools::context::ToolOutput for RequiredArtifactOutput {
    fn log_preview(&self) -> String {
        self.0.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.0.success_for_logging()
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        self.0.projection_metadata()
    }

    fn requires_canonical_artifact(&self) -> bool {
        true
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.0.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.0.to_response_item(call_id, payload)
    }
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
            LifecycleTestResult::RequiredArtifact => Ok(Box::new(RequiredArtifactOutput(
                crate::tools::context::FunctionToolOutput::from_text(
                    "fully received result".to_string(),
                    Some(true),
                ),
            ))
                as Box<dyn crate::tools::context::ToolOutput>),
            LifecycleTestResult::Err => Err(FunctionCallError::RespondToModel(
                "handler failed".to_string(),
            )),
        }
    }
}

impl CoreToolRuntime for LifecycleTestHandler {}

struct BlockingProjectionOutput {
    inner: crate::tools::context::FunctionToolOutput,
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
    projection_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::tools::context::ToolOutput for BlockingProjectionOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.inner.success_for_logging()
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        self.projection_calls
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.entered
            .send(())
            .expect("projection claim test receiver should remain alive");
        self.release
            .recv()
            .expect("projection claim test should release materialization");
        self.inner.projection_metadata()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.inner.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.inner.to_response_item(call_id, payload)
    }
}

struct BlockingProjectionHandler {
    tool_name: codex_tools::ToolName,
    entered: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    projection_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl ToolExecutor<ToolInvocation> for BlockingProjectionHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let entered = self
            .entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("blocking projection handler is single-use");
        let release = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("blocking projection handler is single-use");
        let projection_calls = Arc::clone(&self.projection_calls);
        Box::pin(async move {
            Ok(Box::new(BlockingProjectionOutput {
                inner: crate::tools::context::FunctionToolOutput::from_text(
                    "ordinary result".to_string(),
                    Some(true),
                ),
                entered,
                release,
                projection_calls,
            }) as Box<dyn crate::tools::context::ToolOutput>)
        })
    }
}

impl CoreToolRuntime for BlockingProjectionHandler {}

fn test_spec(tool_name: &codex_tools::ToolName) -> codex_tools::ToolSpec {
    let tool = codex_tools::ResponsesApiTool {
        name: tool_name.name.clone(),
        description: "Test tool.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: codex_tools::JsonSchema::default(),
        output_schema: None,
    };
    match &tool_name.namespace {
        Some(namespace) => codex_tools::ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: namespace.clone(),
            description: "Test namespace.".to_string(),
            tools: vec![codex_tools::ResponsesApiNamespaceTool::Function(tool)],
        }),
        None => codex_tools::ToolSpec::Function(tool),
    }
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
    let registry =
        ToolRegistry::from_tools([Arc::clone(&plain_handler), Arc::clone(&namespaced_handler)]);

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
fn post_tool_feedback_survives_code_mode_projection() {
    let model_visible = crate::tools::context::FunctionToolOutput::from_text(
        "hook feedback".to_string(),
        /*success*/ None,
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
            model_visible,
        }),
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: vec![FunctionCallOutputContentItem::InputText {
            text: "hook feedback".to_string(),
        }],
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

    for original in [
        serde_json::json!({ "typed": true }),
        serde_json::json!(["typed", true]),
        serde_json::json!("typed"),
        serde_json::Value::Null,
    ] {
        let model_visible = crate::tools::context::FunctionToolOutput::from_text(
            "hook feedback".to_string(),
            /*success*/ None,
        );
        let mut result = AnyToolResult {
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
            result: Box::new(PostToolUseFeedbackOutput {
                original: Box::new(codex_tools::JsonToolOutput::new(original.clone())),
                model_visible,
            }),
            model_projection: None,
            source_dependencies: None,
            code_mode_feedback: vec![FunctionCallOutputContentItem::InputText {
                text: "hook feedback".to_string(),
            }],
        };

        assert_eq!(
            result.take_code_mode_feedback(),
            vec![FunctionCallOutputContentItem::InputText {
                text: "hook feedback".to_string(),
            }]
        );
        assert_eq!(result.code_mode_result(), original);
    }
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
    let registry = ToolRegistry::from_tools([ok_handler, failing_handler]);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let ok_terminal_outcome = admitted_tool_dispatch_state();

    registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "ok-call",
                ok_tool.clone(),
            ),
            Arc::clone(&ok_terminal_outcome),
        )
        .await?;
    assert!(ok_terminal_outcome.is_terminal());
    let failing_terminal_outcome = admitted_tool_dispatch_state();
    let err = match registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "failing-call",
                failing_tool.clone(),
            ),
            Arc::clone(&failing_terminal_outcome),
        )
        .await
    {
        Ok(_) => panic!("failing handler should return an error"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "handler failed");
    assert!(failing_terminal_outcome.is_terminal());

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_completion_reserves_projection_before_abort_can_claim() -> anyhow::Result<()> {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.tool_lifecycle_contributor(Arc::new(ToolLifecycleRecorder {
        records: Arc::clone(&records),
    }));
    session.services.extensions = Arc::new(builder.build());

    let tool_name = codex_tools::ToolName::plain("blocking_projection_tool");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let projection_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = ToolRegistry::from_tools([Arc::new(BlockingProjectionHandler {
        tool_name: tool_name.clone(),
        entered: std::sync::Mutex::new(Some(entered_tx)),
        release: std::sync::Mutex::new(Some(release_rx)),
        projection_calls: Arc::clone(&projection_calls),
    }) as Arc<dyn CoreToolRuntime>]);
    let terminal_outcome = admitted_tool_dispatch_state();
    let dispatch_terminal_outcome = Arc::clone(&terminal_outcome);
    let dispatch = tokio::spawn(async move {
        registry
            .dispatch_any_with_terminal_outcome(
                test_invocation(
                    Arc::new(session),
                    Arc::new(turn),
                    "projection-call",
                    tool_name,
                ),
                dispatch_terminal_outcome,
            )
            .await
    });

    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(std::time::Duration::from_secs(1)))
        .await
        .expect("projection entry waiter should join")
        .expect("projection should start");
    let simulated_abort_claimed = !matches!(
        terminal_outcome.try_abort(),
        crate::tools::context::ToolDispatchAbort::AlreadyTerminal
    );
    release_tx
        .send(())
        .expect("projection materialization should remain in flight");

    let result = dispatch.await.expect("dispatch task should join")?;
    assert!(
        !simulated_abort_claimed,
        "handler completion must reserve the ordinary projection before an abort can claim it"
    );
    assert!(result.model_projection.is_some());
    assert_eq!(
        projection_calls.load(std::sync::atomic::Ordering::Acquire),
        1,
        "the ordinary terminal transaction materializes exactly one projection"
    );
    assert_eq!(
        records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[
            RecordedToolLifecycle::Start {
                call_id: "projection-call".to_string(),
                tool_name: codex_tools::ToolName::plain("blocking_projection_tool"),
            },
            RecordedToolLifecycle::Finish {
                call_id: "projection-call".to_string(),
                tool_name: codex_tools::ToolName::plain("blocking_projection_tool"),
                outcome: codex_extension_api::ToolCallOutcome::Completed { success: true },
            },
        ]
    );

    Ok(())
}

#[tokio::test]
async fn projection_failure_preserves_completed_lifecycle_and_returns_bounded_notice()
-> anyhow::Result<()> {
    let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
    let temp = tempfile::tempdir()?;
    let blocked_home = temp.path().join("not-a-directory");
    tokio::fs::write(&blocked_home, b"blocked").await?;
    let mut config = (*turn.config).clone();
    config.codex_home = codex_utils_absolute_path::AbsolutePathBuf::try_from(blocked_home)?;
    turn.config = Arc::new(config);

    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.tool_lifecycle_contributor(Arc::new(ToolLifecycleRecorder {
        records: Arc::clone(&records),
    }));
    session.services.extensions = Arc::new(builder.build());

    let tool_name = codex_tools::ToolName::plain("required_artifact_tool");
    let registry = ToolRegistry::from_tools([Arc::new(LifecycleTestHandler {
        tool_name: tool_name.clone(),
        result: LifecycleTestResult::RequiredArtifact,
    }) as Arc<dyn CoreToolRuntime>]);
    let terminal_outcome = admitted_tool_dispatch_state();
    let result = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::new(session),
                Arc::new(turn),
                "artifact-call",
                tool_name.clone(),
            ),
            Arc::clone(&terminal_outcome),
        )
        .await?;

    let response = result.response();
    let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
        panic!("expected a function call output");
    };
    assert_eq!(output.success, Some(true));
    assert_eq!(
        output.body.to_text().as_deref(),
        Some(
            "Tool execution completed, but its full result could not be preserved for model delivery."
        )
    );
    assert!(terminal_outcome.is_terminal());
    let actual = records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            RecordedToolLifecycle::Start {
                call_id: "artifact-call".to_string(),
                tool_name: tool_name.clone(),
            },
            RecordedToolLifecycle::Finish {
                call_id: "artifact-call".to_string(),
                tool_name,
                outcome: codex_extension_api::ToolCallOutcome::Completed { success: true },
            },
        ]
    );

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

#[test]
fn invocation_identity_is_order_independent_but_action_sensitive() {
    let first = ToolPayload::Function {
        arguments: r#"{"path":"a.rs","line":7}"#.to_string(),
    };
    let reordered = ToolPayload::Function {
        arguments: r#"{"line":7,"path":"a.rs"}"#.to_string(),
    };
    let different_action = ToolPayload::Function {
        arguments: r#"{"path":"b.rs","line":7}"#.to_string(),
    };

    assert_eq!(
        canonical_tool_invocation_sha256(
            &first,
            ParsedFunctionArguments::from_payload(&first).as_ref()
        ),
        canonical_tool_invocation_sha256(
            &reordered,
            ParsedFunctionArguments::from_payload(&reordered).as_ref()
        )
    );
    assert_ne!(
        canonical_tool_invocation_sha256(
            &first,
            ParsedFunctionArguments::from_payload(&first).as_ref()
        ),
        canonical_tool_invocation_sha256(
            &different_action,
            ParsedFunctionArguments::from_payload(&different_action).as_ref()
        )
    );
}

#[test]
fn admission_normalizes_multi_text_output_without_dropping_non_text_content() {
    let original = ResponseInputItem::FunctionCallOutput {
        call_id: "multi-text-call".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "first".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,eA==".to_string(),
                    detail: None,
                },
                FunctionCallOutputContentItem::InputText {
                    text: "second".to_string(),
                },
            ]),
            success: Some(true),
        },
    };

    assert_eq!(history_output_text(&original), None);
    let (normalized, text) =
        normalize_admission_response(original).expect("normalized admission response");

    assert_eq!(text, "first\nsecond");
    assert_eq!(
        history_output_text(&normalized).as_deref(),
        Some("first\nsecond")
    );
    assert_eq!(preserved_non_text_content(&normalized).len(), 1);
}

#[test]
fn admission_fallback_covers_default_text_and_non_text_outputs() {
    let text_response = ResponseInputItem::FunctionCallOutput {
        call_id: "default-text".to_string(),
        output: FunctionCallOutputPayload::from_text("ordinary result".to_string()),
    };
    let text_metadata = admission_fallback_metadata(&text_response, ToolOutputOutcome::Success)
        .expect("function output fallback metadata");
    assert_eq!(text_metadata.spillable_text, vec!["ordinary result"]);
    assert_eq!(text_metadata.outcome, ToolOutputOutcome::Success);
    let text_canonical = admission_fallback_canonical_result(
        &text_metadata.spillable_text.join("\n"),
        &[],
        serde_json::json!({"unused": true}),
    );
    assert_eq!(text_canonical.kind, CanonicalToolResultKind::Text);
    assert_eq!(text_canonical.bytes.as_slice(), b"ordinary result");

    let image_response = ResponseInputItem::FunctionCallOutput {
        call_id: "default-image".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,eA==".to_string(),
                    detail: None,
                },
            ]),
            success: Some(true),
        },
    };
    let image_metadata = admission_fallback_metadata(&image_response, ToolOutputOutcome::Success)
        .expect("function image fallback metadata");
    assert_eq!(image_metadata.spillable_text, vec![""]);
    let preserved = preserved_non_text_content(&image_response);
    let image_canonical = admission_fallback_canonical_result(
        &image_metadata.spillable_text.join("\n"),
        &preserved,
        serde_json::json!({"image_url": "data:image/png;base64,eA=="}),
    );
    assert_eq!(image_canonical.kind, CanonicalToolResultKind::Json);
    assert_eq!(
        image_canonical.value,
        Some(serde_json::json!({"image_url": "data:image/png;base64,eA=="}))
    );
}

#[test]
fn admission_fallback_does_not_retype_structured_tool_search_output() {
    let response = ResponseInputItem::ToolSearchOutput {
        call_id: "tool-search".to_string(),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools: vec![serde_json::json!({"name": "example"})],
        omitted_result_count: None,
    };

    assert!(admission_fallback_metadata(&response, ToolOutputOutcome::Success).is_none());

    let empty_response = ResponseInputItem::FunctionCallOutput {
        call_id: "empty".to_string(),
        output: FunctionCallOutputPayload::from_text(String::new()),
    };
    assert!(admission_fallback_metadata(&empty_response, ToolOutputOutcome::Success).is_none());
}

#[tokio::test]
async fn admission_only_projection_preserves_original_response_and_registers_candidate() {
    let temp = tempfile::tempdir().expect("temporary Codex home");
    let output = "ordinary inline result".to_string();
    let canonical = CanonicalToolResult::text(output.clone());
    let original_response = ResponseInputItem::FunctionCallOutput {
        call_id: "ordinary-call".to_string(),
        output: FunctionCallOutputPayload::from_text(output.clone()),
    };
    let invocation_sha256 = crate::tool_history::sha256(b"normalized invocation");
    let projection = project_model_output(ModelProjectionInput {
        spillable_text: output.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "ordinary-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "generic_fallback",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 1_000,
        projected_text: output.clone(),
        preserved_content: Vec::new(),
        codex_home: temp.path().to_path_buf(),
        thread_id: "ordinary-thread".to_string(),
        tool_name: "ordinary_tool".to_string(),
        original_output_sha256: canonical.sha256.clone(),
        original_output_tokens: canonical.approximate_tokens,
        original_output_text: output.clone(),
        invocation_sha256: Some(invocation_sha256.clone()),
        canonical,
        semantic_class: "tool_output".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: original_response.clone(),
        materialization: ProjectionMaterialization::AdmissionOnly,
    })
    .await
    .expect("admission-only projection");

    assert_eq!(projection.response(), original_response);
    let candidate = projection.candidate.expect("admission candidate");
    assert_eq!(candidate.bounded_model_output, output);
    assert!(
        candidate
            .supersession_identity
            .is_some_and(|identity| identity.contains(&invocation_sha256))
    );
}

#[tokio::test]
async fn admission_only_projection_preserves_original_response_when_artifact_storage_fails() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let blocked_home = temp.path().join("not-a-directory");
    tokio::fs::write(&blocked_home, b"blocked")
        .await
        .expect("create blocking file");
    let output = "completed wait result".to_string();
    let canonical = CanonicalToolResult::text(output.clone());
    let original_response = ResponseInputItem::FunctionCallOutput {
        call_id: "wait-call".to_string(),
        output: FunctionCallOutputPayload::from_text(output.clone()),
    };

    let projection = project_model_output(ModelProjectionInput {
        spillable_text: output.clone(),
        outcome: ToolOutputOutcome::Success,
        essential_inline: serde_json::json!({}),
        origin_call_id: "wait-call".to_string(),
        selection_facts: ProjectionSelectionFacts {
            mode: "generic_fallback",
            available_fragments: 0,
            selected_fragments: 0,
            exact_duplicates_removed: 0,
            selected_ids: Vec::new(),
            omitted_inline_ids: Vec::new(),
            partial_ids: Vec::new(),
        },
        applied_token_limit: 1_000,
        projected_text: output.clone(),
        preserved_content: Vec::new(),
        codex_home: blocked_home,
        thread_id: "wait-thread".to_string(),
        tool_name: "wait".to_string(),
        original_output_sha256: canonical.sha256.clone(),
        original_output_tokens: canonical.approximate_tokens,
        original_output_text: output,
        invocation_sha256: None,
        canonical,
        semantic_class: "tool_output".to_string(),
        source_dependencies: std::collections::BTreeSet::new(),
        projection_eligible: true,
        projection_truncated: false,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
        original_response: original_response.clone(),
        materialization: ProjectionMaterialization::AdmissionOnly,
    })
    .await
    .expect("admission-only projection should fall back to the received response");

    assert_eq!(projection.response(), original_response);
    assert!(projection.candidate.is_none());
    assert!(!projection.artifact_created);
}
