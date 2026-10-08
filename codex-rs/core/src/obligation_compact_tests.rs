use super::*;

#[test]
fn obligation_rebase_cannot_launder_a_constraint_through_evidence() {
    let anchor = "Do not modify production files.";
    let unresolved = format!("{anchor} {}", "must preserve qualifier ".repeat(60));
    let previous = format!("{SUMMARY_PREFIX}\n## Goal\nDiagnose.\n## Current state\nInvestigating.\n## Completed work\nRead source.\n## Unresolved work\n{unresolved}\n## Evidence\n{}\n## Next action\nInspect.", "prior observation ".repeat(120));
    let suffix = format!("## Unresolved work\nNone.\n## Evidence\nQuoted earlier text: {unresolved}\n{}\n## Next action\nInspect.", "new unrelated observation ".repeat(40));
    let sections = compaction_rebase_sections(&previous);
    assert!(sections.contains(&3) && sections.contains(&4));
    let first = validated_rebased_compaction_summary(&previous, &suffix, &sections).unwrap();
    assert!(approx_token_count(&first) <= COMPACT_TASK_STATE_MAX_TOKENS);
    let sections = compaction_rebase_sections(&first);
    // Account for the mistaken empty-work claim, not the prohibition.
    // Conservation deliberately does not interpret the prose "None.".
    let second = format!("## Goal\nDiagnose.\n## Current state\nInvestigating.\n## Completed work\nRead source.\nResolved: None. Evidence: corrected the mistaken empty-work claim.\n## Unresolved work\n{unresolved}\n## Evidence\nFresh observations only.\n## Next action\nInspect.");
    let retained = validated_rebased_compaction_summary(&first, &second, &sections).unwrap();
    assert!(retained.contains(anchor));
    assert!(approx_token_count(&retained) <= COMPACT_TASK_STATE_MAX_TOKENS);
    let mut section = None;
    assert!(checkpoint_lines(&first).any(|(line, heading)| {
        if let Some(index) = heading { section = Some(index); }
        section == Some(3) && line.contains(anchor)
    }), "evidence quotations must not discharge unresolved scope");
}

#[test]
fn obligation_recent_short_correction_survives_saturated_old_notes_and_bulk() {
    let original = "Original request: examine the implementation.";
    let correction = "LATEST-CORRECTION: diagnosis only; do not edit or install anything.";
    let mut messages = vec![compacted_user_message(original)];
    for index in 0..50 {
        messages.push(compacted_user_message(&format!("Earlier note {index}: {}", "background detail ".repeat(80))));
    }
    messages.push(compacted_user_message(correction));
    messages.push(compacted_user_message(&"bulk log data ".repeat(10_000)));
    let (history, _, _, _) = append_bounded_user_messages(Vec::new(), &messages, COMPACT_USER_MESSAGE_MAX_TOKENS, 0, 0);
    let retained = collect_user_messages(&history);
    assert!(retained.iter().any(|message| message == &compacted_user_message(original)));
    assert!(retained.iter().any(|message| message == &compacted_user_message(correction)));
    assert!(retained.iter().map(compacted_user_message_text_tokens).sum::<usize>() <= COMPACT_USER_MESSAGE_MAX_TOKENS);
}

#[test]
fn obligation_short_text_parts_are_not_starved_by_bulk_in_the_same_message() {
    let correction = "MULTIPART-CORRECTION: do not write files.";
    let bulk = "large attachment ".repeat(10_000);
    let mut mixed = compacted_user_message(&bulk);
    mixed.content.push(UserInput::Text { text: correction.into(), text_elements: Vec::new() });
    let messages = vec![compacted_user_message("Original request: review only."), mixed,
        compacted_user_message(&"newer log data ".repeat(10_000))];
    let (history, _, _, _) = append_bounded_user_messages(Vec::new(), &messages, COMPACT_USER_MESSAGE_MAX_TOKENS, 0, 0);
    let retained = collect_user_messages(&history);
    assert!(retained.iter().flat_map(|message| &message.content).any(|part|
        matches!(part, UserInput::Text { text, .. } if text == correction)));
    assert!(retained.iter().map(compacted_user_message_text_tokens).sum::<usize>() <= COMPACT_USER_MESSAGE_MAX_TOKENS);
}
