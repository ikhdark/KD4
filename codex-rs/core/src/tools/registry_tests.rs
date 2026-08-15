use super::*;
use crate::session::step_context::StepContext;
use pretty_assertions::assert_eq;

#[test]
fn nested_code_mode_projection_is_not_provider_visible() {
    assert!(projection_is_provider_visible(&ToolCallSource::Direct));
    assert!(!projection_is_provider_visible(&ToolCallSource::CodeMode {
        cell_id: "cell".to_string(),
        runtime_tool_call_id: "nested".to_string(),
    }));
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

    let projected = serialize_projection_with_limit(envelope, &output, 64).expect("projection");
    let rendered = projected.rendered();

    assert!(matches!(
        &projected,
        BoundedModelProjection::Envelope { .. }
    ));
    assert!(approx_token_count(rendered) <= 64);
    assert_eq!(
        serde_json::from_str::<Value>(rendered).expect("valid JSON projection"),
        projected.value()
    );
}

#[test]
fn projection_limit_retains_a_native_minimal_fallback() {
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
        result: serde_json::json!({ "large_metadata": "x".repeat(8_000) }),
    };

    let projected =
        serialize_projection_with_limit(envelope, "selected output", 1).expect("fallback");
    let BoundedModelProjection::Fallback { value, rendered } = projected else {
        panic!("expected a bounded fallback");
    };

    assert!(approx_token_count(&rendered) <= 1);
    assert_eq!(
        serde_json::from_str::<Value>(&rendered).expect("valid JSON fallback"),
        value
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
        semantic_class: "test".to_string(),
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
        canonical,
        semantic_class: "tool_output".to_string(),
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
    let envelope: ToolProjectionV1 = serde_json::from_str(&rendered).expect("projection envelope");
    let recovery = envelope.result["preserved_content"]
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
    let canonical = CanonicalToolResult::text(full_output.clone());
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
        canonical,
        semantic_class: "test_projection".to_string(),
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
    let envelope: ToolProjectionV1 = serde_json::from_str(&rendered).expect("projection envelope");
    let recovery = envelope.result["preserved_content"]
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
        semantic_class: "tool_output".to_string(),
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
    let inner_canonical = CanonicalToolResult::json(serde_json::json!({
        "line": "exact nested evidence",
        "pointer": {"blocker": "exact pointer evidence"},
    }));
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
        semantic_class: "test_projection".to_string(),
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
        post_tool_use_payload: None,
        model_projection: Some(inner_projection),
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
    assert_eq!(
        nested.code_mode_result(),
        Value::String("native nested value".to_string())
    );

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
        semantic_class: "tool_output".to_string(),
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
        post_tool_use_payload: None,
        model_projection: Some(outer_projection),
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
    let envelope: ToolProjectionV1 = serde_json::from_str(&rendered).expect("outer envelope");
    assert!(
        envelope.result["preserved_content"]
            .to_string()
            .contains("exact nested evidence")
    );
    assert!(
        envelope.result["preserved_content"]
            .to_string()
            .contains("exact pointer evidence")
    );
    assert!(
        !envelope.result["preserved_content"]
            .to_string()
            .contains(&"x".repeat(8_000))
    );
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
        post_tool_use_payload: None,
        model_projection: None,
    };

    assert!(result.owner_drained_continuation().is_none());
}

#[tokio::test]
async fn missing_stale_and_oversized_predetermined_artifacts_fail_open() {
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
    let ok_terminal_outcome = Arc::new(AtomicBool::new(false));

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
    assert!(ok_terminal_outcome.load(Ordering::Acquire));
    let failing_terminal_outcome = Arc::new(AtomicBool::new(false));
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
    assert!(failing_terminal_outcome.load(Ordering::Acquire));

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
