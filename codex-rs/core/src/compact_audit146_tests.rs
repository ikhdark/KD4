use super::*;
use codex_protocol::models::FunctionCallOutputPayload;

fn checkpoint(goal: &str, unresolved: &str) -> String {
    format!(
        "## Goal\n{goal}\n\n## Current state\nActive\n\n## Completed work\nNone\n\n## Unresolved work\n{unresolved}\n\n## Evidence\nNone\n\n## Next action\nContinue"
    )
}

fn message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn call(id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "read_file".into(),
        namespace: None,
        arguments: "{}".into(),
        call_id: id.into(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn output(id: &str, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: id.into(),
        output: FunctionCallOutputPayload::from_text(text.into()),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn audit146_first_request_reserves_prefix_without_overriding_custom_input() {
    let mut input = vec![UserInput::Text {
        text: SUMMARIZATION_PROMPT.into(),
        text_elements: Vec::new(),
    }];
    select_compaction_input(&mut input, None, None, &[]);
    let allowance = COMPACT_TASK_STATE_MAX_TOKENS - approx_token_count(SUMMARY_PREFIX) - 1;
    assert!(matches!(&input[0], UserInput::Text { text, .. }
        if text.contains(&format!("at most {allowance} tokens"))));
    let base = checkpoint("Finish the task", "Preserve constraints");
    let fitting = format!("{base}{}", "x".repeat(allowance * 4 - base.len()));
    let bounded = validated_compaction_summary(None, &fitting, true, false).unwrap();
    assert!(bounded.ends_with(&fitting));
    assert!(approx_token_count(&bounded) <= COMPACT_TASK_STATE_MAX_TOKENS);
    let original = vec![UserInput::Text {
        text: "Use my exact format".into(),
        text_elements: Vec::new(),
    }];
    let mut custom = original.clone();
    select_compaction_input(&mut custom, None, Some("custom"), &[]);
    assert!(matches!(&custom[0], UserInput::Text { text, .. } if text == "Use my exact format"));
}

#[test]
fn audit146_inline_code_is_not_an_atomic_fence() {
    for text in [
        "`src/file.rs` changed",
        "``quoted`` changed",
        "~~ordinary text",
    ] {
        assert!(checkpoint_lines_with_structure(text).all(|(_, _, outer)| outer));
        let body = format!("Keep this obligation.\n\n{text} {}", "padding ".repeat(200));
        let retained = retain_newest_section_updates(&[(body, 0)], 20);
        assert_eq!(retained, "Keep this obligation.");
    }
    let fenced = format!("```text\n{}\n```", "quoted ".repeat(200));
    assert!(retain_newest_section_updates(&[(fenced, 0)], 20).is_empty());
}

#[test]
fn audit146_rebase_requires_whole_anchor_lines() {
    let previous = format!(
        "{SUMMARY_PREFIX}\n{}",
        checkpoint("Do not run tests", "Finish verification")
    );
    let suffix = checkpoint(
        "Retire constraint: Do not run tests",
        "Quoted: Finish verification",
    );
    let rebased =
        validated_rebased_compaction_summary(&previous, &suffix, &[0, 1, 2, 3, 4, 5]).unwrap();
    assert!(rebased.lines().any(|line| line == "Do not run tests"));
    assert!(rebased.lines().any(|line| line == "Finish verification"));
    assert!(!compaction_anchor_accounted_for(
        "Resolved: unrelated Do not run tests Evidence: receipt",
        "Do not run tests"
    ));
    assert!(compaction_anchor_accounted_for(
        "Resolved: Do not run tests Evidence: receipt",
        "Do not run tests"
    ));
}

#[test]
fn audit146_legacy_pairing_retains_only_the_unconsumed_occurrence() {
    for newer_id in ["same", "different"] {
        let items = vec![
            call("same"),
            output("same", "old"),
            message("assistant", "Observed old result"),
            call(newer_id),
            output(newer_id, "new"),
        ];
        assert_eq!(unresolved_compaction_start(&items), 3);
        assert_eq!(compaction_summary_items(&items), items[..3]);
        let (retained, _, _) = build_task_input_checkpoint(&items);
        let tools = retained
            .iter()
            .filter(|item| {
                compaction_call_id(item).is_some() || compaction_output_call_id(item).is_some()
            })
            .collect::<Vec<_>>();
        assert_eq!(tools, vec![&items[3], &items[4]]);
    }
    let ambiguous = vec![call("same"), call("same"), output("same", "ambiguous")];
    assert_eq!(unresolved_compaction_start(&ambiguous), 0);
}

#[test]
fn audit146_custom_headings_are_not_the_default_schema() {
    let custom = "## Goal\nKeep constraints\n\n## Status\nStill working";
    assert!(validated_compaction_summary(None, custom, true, true).is_ok());
    assert!(validated_compaction_summary(None, custom, true, false).is_err());
    assert!(validated_compaction_summary(None, "", true, true).is_err());
}

#[test]
fn audit146_image_only_eviction_requires_source_recovery() {
    for count in [MAX_RETAINED_USER_IMAGES, MAX_RETAINED_USER_IMAGES + 1] {
        let images = ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: (0..count)
                .map(|index| ContentItem::InputImage {
                    image_url: format!("https://example.invalid/{index}.png"),
                    detail: None,
                })
                .collect(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let (_, retained, needs_recovery) = build_task_input_checkpoint(&[images]);
        assert_eq!(retained, MAX_RETAINED_USER_IMAGES);
        assert_eq!(needs_recovery, count > MAX_RETAINED_USER_IMAGES);
    }
}

#[tokio::test]
async fn audit146_local_image_only_eviction_archives_all_source_images() {
    for count in [8, 9] {
        let source = vec![ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: (0..count)
                .map(|index| ContentItem::InputImage {
                    image_url: format!("https://example.invalid/{index}.png"),
                    detail: None,
                })
                .collect(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        let tasks = task_compaction_items(&source);
        let (_, _, omitted_images, _, omitted_text) = build_bounded_input_history(&tasks, false);
        let recovery = local_compaction_source_recovery(
            tasks,
            &compaction_summary_items(&source),
            omitted_text || omitted_images > 0,
        );
        if count == 8 {
            assert!(recovery.is_none());
            continue;
        }
        let archived = recovery.unwrap().await.unwrap();
        assert_eq!(
            archived.value.unwrap()["items"],
            serde_json::to_value(&source).unwrap()
        );
    }
}

#[test]
fn audit146_bounded_goal_marker_does_not_block_the_next_rebase() {
    let paragraphs = |label: &str, count: usize| {
        (0..count)
            .map(|index| format!("{label} {index}: {}", "keep this exact qualification ".repeat(10)))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    // A first handoff past the budget is bounded by the runtime, which keeps
    // only the newest Goal paragraphs under an incomplete-excerpt marker.
    let oversized = format!(
        "{GOAL_HEADING}\n{}\n\n{CURRENT_STATE_HEADING}\n{}\n\n{COMPLETED_WORK_HEADING}\n{}\n\n{UNRESOLVED_WORK_HEADING}\nFinish verification\n\n{EVIDENCE_HEADING}\n{}\n\n{NEXT_ACTION_HEADING}\n{}",
        paragraphs("Goal constraint", 20),
        paragraphs("State", 10),
        paragraphs("Done", 8),
        paragraphs("Fact", 15),
        paragraphs("Step", 5),
    );
    let first = validated_compaction_summary(None, &oversized, true, false).unwrap();
    assert!(
        first.contains(&format!("{GOAL_HEADING}\n{INCOMPLETE_CHECKPOINT_EXCERPT}\n")),
        "fixture must bound the Goal: {first}"
    );
    let rebased = compaction_rebase_sections(&first);
    assert_eq!(rebased, (0..COMPACTION_SECTIONS.len()).collect::<Vec<_>>());
    let kept_goal = first
        .lines()
        .skip_while(|line| *line != GOAL_HEADING)
        .skip(1)
        .take_while(|line| *line != CURRENT_STATE_HEADING)
        .filter(|line| *line != INCOMPLETE_CHECKPOINT_EXCERPT)
        .collect::<Vec<_>>()
        .join("\n");
    let kept_goal = kept_goal.trim();
    // A compliant complete rebase keeps every earlier Goal line verbatim. The
    // runtime marker is not an obligation, so it is neither required nor re-added.
    let summary = validated_rebased_compaction_summary(
        &first,
        &checkpoint(kept_goal, "Finish verification"),
        &rebased,
    )
    .unwrap();
    assert!(!summary.lines().any(|line| line == INCOMPLETE_CHECKPOINT_EXCERPT));
    assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    // Anchor conservation still holds: an omitted Goal line is restored.
    let (dropped, rest) = kept_goal
        .split_once("\n\n")
        .expect("several Goal paragraphs survive the bound");
    let repaired = validated_rebased_compaction_summary(
        &first,
        &checkpoint(rest, "Finish verification"),
        &rebased,
    )
    .unwrap();
    assert!(repaired.lines().any(|line| line == dropped));
    // Negative control: a marker authored in the fresh update is still rejected.
    let copied = checkpoint(
        &format!("{INCOMPLETE_CHECKPOINT_EXCERPT}\n{kept_goal}"),
        "Finish verification",
    );
    assert!(validated_rebased_compaction_summary(&first, &copied, &rebased).is_err());
}

#[test]
fn audit146_corrective_rebase_bounds_conserved_anchors_instead_of_failing() {
    let anchors = |label: &str| {
        (0..12)
            .map(|index| format!("{label} {index}: {}", "retain this binding qualification ".repeat(8)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (goal, unresolved) = (anchors("Constraint"), anchors("Obligation"));
    let previous = format!("{SUMMARY_PREFIX}\n{}", checkpoint(&goal, &unresolved));
    assert!(approx_token_count(&previous) <= COMPACT_TASK_STATE_MAX_TOKENS);
    let rebased = compaction_rebase_sections(&previous);
    assert_eq!(rebased, (0..COMPACTION_SECTIONS.len()).collect::<Vec<_>>());
    // A compliant rebase: every anchor verbatim, plus the new segment's state.
    // Accounting for an anchor instead would cost more than keeping it.
    let refreshed = |evidence: &str| {
        format!(
            "{GOAL_HEADING}\n{goal}\n\n{CURRENT_STATE_HEADING}\nVerification running\n\n{COMPLETED_WORK_HEADING}\nApplied the fix\n\n{UNRESOLVED_WORK_HEADING}\n{unresolved}\nObligation new: confirm the follow-up\n\n{EVIDENCE_HEADING}\n{evidence}\n\n{NEXT_ACTION_HEADING}\nRun the focused proof"
        )
    };
    let suffix = refreshed(&"Observed fact with its source and qualification. ".repeat(40));
    let error = validated_rebased_compaction_summary(&previous, &suffix, &rebased)
        .expect_err("the first attempt still requests a complete rebase");
    assert!(error.to_string().contains("needs a complete rebase"), "{error}");
    // The corrective attempt is the last one; it bounds instead of failing.
    let bounded = rebased_compaction_summary(&previous, &suffix, &rebased, true).unwrap();
    assert!(approx_token_count(&bounded) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(bounded.lines().any(|line| line == INCOMPLETE_CHECKPOINT_EXCERPT));
    let recovery = generated_summary_recovery_canonical(Some(previous.as_str()), &suffix, &bounded)
        .expect("a bounded checkpoint keeps both exact handoffs");
    assert_eq!(
        recovery.value.as_ref().unwrap()["items"],
        serde_json::json!([previous, suffix])
    );
    // Negative control: an update that fits is identical in both modes.
    let fitting = refreshed("Observed fact with its source.");
    let strict = validated_rebased_compaction_summary(&previous, &fitting, &rebased).unwrap();
    assert_eq!(
        rebased_compaction_summary(&previous, &fitting, &rebased, true).unwrap(),
        strict
    );
    assert!(!strict.contains(INCOMPLETE_CHECKPOINT_EXCERPT));
}

#[test]
fn audit146_summary_items_keep_a_tail_phase_checkpoint() {
    let checkpoint = crate::tool_history::pressure_retirement_checkpoint(
        &["read".to_string()],
        crate::tool_history::PressureRetirementTrigger::ContextLimit,
    );
    let items = vec![
        message("user", "request"),
        call("read"),
        output("read", "consumed evidence"),
        message("assistant", "Observed the evidence"),
        checkpoint,
        message("user", "new constraint"),
    ];
    // The checkpoint retires consumed output above it, so a pinned summarizer
    // input needs it; an ordinary unread tail message still stays out.
    assert_eq!(compaction_summary_items(&items), items[..5]);
}

#[test]
fn audit146_summarizer_input_stays_exact_unless_it_exceeds_the_window() {
    let prompt = Prompt {
        input: vec![call("read"), output("read", &"exact evidence ".repeat(4_000))].into(),
        ..Default::default()
    };
    let needed = prompt.budget_tokens_for_input(&prompt.input);
    assert!(!local_compaction_input_exceeds_window(&prompt, None));
    assert!(!local_compaction_input_exceeds_window(
        &prompt,
        Some(needed + 1)
    ));
    assert!(local_compaction_input_exceeds_window(&prompt, Some(needed)));
}
