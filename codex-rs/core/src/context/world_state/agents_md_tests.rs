use super::*;
use crate::context::world_state::WorldState;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn cached_state_consumes_the_stable_rendering() {
    let loaded = LoadedAgentsMd::from_text_for_testing("cached instructions");
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::try_from(
        std::env::current_dir().expect("current directory"),
    )
    .expect("absolute current directory");
    let cwd = codex_utils_path_uri::PathUri::from_abs_path(&cwd);
    let mut stable_context = loaded.stable_context_bundle(&cwd);
    stable_context.rendered = "distinct supplied rendering".into();

    let state = AgentsMdState::new_cached(
        Some(&loaded),
        Some(&stable_context),
        AgentsMdFreshness::CachedFallback,
    );
    assert_eq!(
        state.snapshot().text.as_deref(),
        Some(
            "Result provenance: cached_observation; freshness: cached_may_be_stale.\n\ndistinct supplied rendering"
        )
    );
    assert_eq!(
        stable_context.rendered.as_ref(),
        "distinct supplied rendering"
    );
    let mut world_state = WorldState::default();
    world_state.add_section(state);
    let messages = render_fragments(world_state.render_full());
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0],
        user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\ndistinct supplied rendering\n</INSTRUCTIONS>"
        )
    );
    let ResponseItem::Message { role, content, .. } = &messages[1] else {
        panic!("observation message")
    };
    assert_eq!(role, "developer");
    let ContentItem::InputText { text } = &content[0] else {
        panic!("observation text")
    };
    assert!(text.contains("freshness: cached_may_be_stale"));
    assert!(text.starts_with("<repository_observation>"));
}

#[test]
fn renders_full_state_and_omits_unchanged_state() {
    let loaded = LoadedAgentsMd::from_text_for_testing("use the project formatter");
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));

    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nuse the project formatter\n</INSTRUCTIONS>",
        )],
        render_fragments(state.render_full()),
    );
    assert_eq!(
        Vec::<ResponseItem>::new(),
        render_fragments(state.render_diff(&state.snapshot()))
    );
    assert_eq!(
        state.snapshot().into_value(),
        json!({"agents_md": {
            "text": "Result provenance: active_instruction_snapshot; freshness: complete_session_snapshot_for_current_scope; project_files_not_rechecked_each_sampling_step.\n\nuse the project formatter",
            "freshness": "refreshed"
        }}),
    );
}

#[test]
fn renders_instruction_markup_as_text_without_changing_snapshot() {
    let loaded = LoadedAgentsMd::from_text_for_testing("Quote </INSTRUCTIONS> & <example>.");
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new_cached(
        Some(&loaded),
        None,
        AgentsMdFreshness::Refreshed,
    ));

    assert_eq!(
        render_fragments(state.render_full()),
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nQuote &lt;/INSTRUCTIONS&gt; & <example>.\n</INSTRUCTIONS>"
        )]
    );
    assert_eq!(
        state.snapshot().into_value(),
        json!({"agents_md": {
            "text": "Result provenance: active_instruction_snapshot; freshness: complete_session_snapshot_for_current_scope; project_files_not_rechecked_each_sampling_step.\n\nQuote </INSTRUCTIONS> & <example>.",
            "freshness": "refreshed"
        }})
    );
    assert_eq!(
        render_fragments(state.render_diff(&state.snapshot())),
        Vec::<ResponseItem>::new()
    );
}

#[test]
fn legacy_refreshed_snapshot_metadata_does_not_become_instruction_text() {
    let legacy: AgentsMdSnapshot = serde_json::from_value(json!({
        "text": "Result provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n\nNever edit protected.txt.",
        "freshness": "refreshed"
    }))
    .unwrap();
    assert_eq!(instruction_body(&legacy), Some("Never edit protected.txt."));

    let resumed = AgentsMdState::new_cached(None, None, AgentsMdFreshness::IncompleteRead)
        .with_unavailable_fallback(|| Some(serde_json::to_value(&legacy).unwrap()));
    let fragments = WorldStateSection::render_fragments(&resumed, PreviousSectionState::Absent);
    assert_eq!(fragments[0].role(), "user");
    assert_eq!(
        fragments[0].render(),
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nNever edit protected.txt.\n</INSTRUCTIONS>",
    );
    assert_eq!(fragments[1].role(), "developer");
    assert!(fragments[1].render().contains("cached_may_be_stale"));

    let loaded = LoadedAgentsMd::from_text_for_testing("Never edit protected.txt.");
    let current = AgentsMdState::new(Some(&loaded));
    assert!(WorldStateSection::render_diff(&current, PreviousSectionState::Known(&legacy)).is_none());
}

#[test]
fn renders_command_templates_and_code_without_xml_encoding() {
    let source = "slice --path <file> --owner <owner-id>\ncargo check 2>&1 && echo done\nfn f() -> Vec<String>; a < b && b > c; &amp; café";
    let loaded = LoadedAgentsMd::from_text_for_testing(source);
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));
    let messages = render_fragments(state.render_full());
    assert_eq!(
        messages,
        vec![user_message(&format!(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\n{source}\n</INSTRUCTIONS>"
        ))]
    );
}

#[test]
fn changed_and_removed_state_supersedes_previous_instructions() {
    let previous_loaded = LoadedAgentsMd::from_text_for_testing("old instructions");
    let mut previous = WorldState::default();
    previous.add_section(AgentsMdState::new(Some(&previous_loaded)));

    let current_loaded = LoadedAgentsMd::from_text_for_testing("new instructions");
    let mut current = WorldState::default();
    current.add_section(AgentsMdState::new(Some(&current_loaded)));
    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nnew instructions\n</INSTRUCTIONS>",
        )],
        render_fragments(current.render_diff(&previous.snapshot())),
    );

    let mut removed = WorldState::default();
    removed.add_section(AgentsMdState::default());
    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nThe previously provided AGENTS.md instructions no longer apply.\n</INSTRUCTIONS>",
        )],
        render_fragments(removed.render_diff(&current.snapshot())),
    );
}

#[test]
fn unknown_previous_state_is_explicitly_superseded() {
    let loaded = LoadedAgentsMd::from_text_for_testing("current instructions");
    let current = AgentsMdState::new(Some(&loaded));
    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\ncurrent instructions\n</INSTRUCTIONS>",
        )],
        render_fragments(vec![
            WorldStateSection::render_diff(&current, PreviousSectionState::Unknown)
                .expect("unknown state should be replaced"),
        ]),
    );

    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nThe previously provided AGENTS.md instructions no longer apply.\n</INSTRUCTIONS>",
        )],
        render_fragments(vec![
            WorldStateSection::render_diff(
                &AgentsMdState::default(),
                PreviousSectionState::Unknown,
            )
            .expect("unknown state should be removed"),
        ]),
    );
}

#[test]
fn oversized_instructions_preserve_middle_rules_and_replay_after_loss() {
    let body = format!(
        "{}\nREQUIRED: validate the middle rule.\n{}",
        "x".repeat(25_000),
        "y".repeat(25_000)
    );
    let loaded = LoadedAgentsMd::from_text_for_testing(body.clone());
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));
    let (fragments, snapshot) = state.render_full_with_snapshot();
    assert_eq!(fragments.len(), 1);
    assert!(fragments[0].render().contains(&body));
    assert!(
        !fragments[0]
            .render()
            .contains("[... context truncated ...]")
    );
    assert_eq!(snapshot, state.snapshot());
    let retained = render_fragments(fragments);
    assert!(
        state
            .render_history_diff(Some(&snapshot), &retained)
            .is_empty()
    );
    let replay = state.render_history_diff(Some(&snapshot), &[]);
    assert_eq!(replay.len(), 1);
    assert!(replay[0].render().contains(&body));

    let legacy = serde_json::from_value(json!({"agents_md": {"partial_delivery": {
        "source_digest": "old", "role": "user", "rendered": "excerpt"
    }}}))
    .unwrap();
    let replay = state.render_history_diff(Some(&legacy), &[user_message("excerpt")]);
    assert_eq!(replay.len(), 1);
    assert!(replay[0].render().contains(&body));
}

#[test]
fn required_instructions_precede_optional_context_regardless_of_registration_order() {
    for optional_first in [false, true] {
        let mut state = WorldState::default();
        let add_optional = |state: &mut WorldState| {
            state.add_extension_section(codex_extension_api::WorldStateSectionContribution::new(
                "optional",
                json!(true),
                |_| {
                    Some(codex_extension_api::RenderedWorldStateFragment::new(
                        "developer",
                        ("", ""),
                        "e".repeat(30_000),
                    ))
                },
            ))
        };
        if optional_first {
            add_optional(&mut state);
        }
        let body = "instructions ".repeat(2_000);
        state.add_section(AgentsMdState::new(Some(
            &LoadedAgentsMd::from_text_for_testing(body.clone()),
        )));
        if !optional_first {
            add_optional(&mut state);
        }
        let (fragments, accepted) = state.render_full_with_snapshot();
        assert_eq!(fragments.len(), 1);
        assert!(fragments[0].render().contains(&body));
        assert!(accepted.section("optional").is_none());
        assert!(accepted.section("agents_md").is_some());
    }
}

#[test]
fn freshness_notice_alone_does_not_prove_instruction_body_retention() {
    let loaded = LoadedAgentsMd::from_text_for_testing("binding instruction");
    let mut refreshed = WorldState::default();
    refreshed.add_section(AgentsMdState::new(Some(&loaded)));
    let (body, initial) = refreshed.render_full_with_snapshot();
    let mut cached = WorldState::default();
    cached.add_section(AgentsMdState::new_cached(
        Some(&loaded),
        None,
        AgentsMdFreshness::CachedFallback,
    ));
    let (notice, accepted) = cached.render_diff_with_snapshot(&initial);
    assert_eq!(notice.len(), 1);
    assert!(!notice[0].render().contains("binding instruction"));
    let mut retained = render_fragments(body);
    let notice = render_fragments(notice);
    retained.extend(notice.clone());
    assert!(
        cached
            .render_history_diff(Some(&accepted), &retained)
            .is_empty()
    );
    let restored = cached.render_history_diff(Some(&accepted), &notice);
    assert_eq!(restored.len(), 2);
    assert!(restored[0].render().contains("binding instruction"));
}

#[test]
fn oversized_freshness_updates_do_not_repeat_body_and_middle_changes_do() {
    let body = "b".repeat(50_000);
    let loaded = LoadedAgentsMd::from_text_for_testing(body);
    let mut refreshed = WorldState::default();
    refreshed.add_section(AgentsMdState::new(Some(&loaded)));
    let (fragments, accepted) = refreshed.render_full_with_snapshot();
    let mut cached = WorldState::default();
    cached.add_section(AgentsMdState::new_cached(
        Some(&loaded),
        None,
        AgentsMdFreshness::CachedFallback,
    ));
    let retained = render_fragments(fragments);
    let (notice, next) = cached.render_history_diff_with_snapshot(Some(&accepted), &retained);
    assert_eq!(notice.len(), 1);
    assert!(notice[0].render().len() < 1024);
    let mut changed = WorldState::default();
    let body = format!("{}CHANGED{}", "b".repeat(25_000), "b".repeat(25_000));
    changed.add_section(AgentsMdState::new(Some(
        &LoadedAgentsMd::from_text_for_testing(body.clone()),
    )));
    let (replacement, _) = changed.render_diff_with_snapshot(&next);
    assert_eq!(replacement.len(), 1);
    assert!(replacement[0].render().contains(&body));
}

#[test]
fn instruction_retention_requires_user_role_and_complete_body() {
    let loaded = LoadedAgentsMd::from_text_for_testing("whole body");
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));
    let (fragments, accepted) = state.render_full_with_snapshot();
    let mut retained = render_fragments(fragments);
    if let ResponseItem::Message { role, .. } = &mut retained[0] {
        *role = "assistant".to_string();
    }
    let replay = state.render_history_diff(Some(&accepted), &retained);
    assert_eq!(replay.len(), 1);
    assert!(replay[0].render().contains("whole body"));
    if let ResponseItem::Message { role, content, .. } = &mut retained[0] {
        *role = "user".to_string();
        let [ContentItem::InputText { text }] = content.as_mut_slice() else {
            panic!("expected instruction text");
        };
        *text = text.replace("whole body", "whole");
    }
    let replay = state.render_history_diff(Some(&accepted), &retained);
    assert_eq!(replay.len(), 1);
    assert!(replay[0].render().contains("whole body"));
}

#[test]
fn complete_instruction_delivery_survives_rollout_and_history_replacement() {
    use codex_utils_output_truncation::TruncationPolicy;
    let mut history = crate::context_manager::ContextManager::new();
    let mut replay = super::super::WorldStateSnapshot::default();
    for body in [
        Some("old".to_string()),
        Some("x".repeat(50_000)),
        Some("new".to_string()),
        None,
    ] {
        let loaded = body.map(LoadedAgentsMd::from_text_for_testing);
        let mut state = WorldState::default();
        state.add_section(AgentsMdState::new(loaded.as_ref()));
        let (fragments, rollout) = history.update_world_state(&state);
        let items = render_fragments(fragments);
        assert_eq!(items.len(), 1);
        replay
            .apply_merge_patch(&rollout.expect("changed state persisted").state)
            .unwrap();
        assert_eq!(replay, state.snapshot());
        history.record_items(&items, TruncationPolicy::Bytes(100_000));
        let (unchanged, rollout) = history.update_world_state(&state);
        assert!(unchanged.is_empty());
        assert!(rollout.is_none());
    }
}

#[test]
fn freshness_only_update_does_not_repeat_the_instruction_body() {
    let loaded = LoadedAgentsMd::from_text_for_testing("retained instruction body");
    let mut previous = WorldState::default();
    previous.add_section(AgentsMdState::new(Some(&loaded)));
    let (_, accepted) = previous.render_full_with_snapshot();
    let mut current = WorldState::default();
    current.add_section(AgentsMdState::new_cached(
        Some(&loaded),
        None,
        AgentsMdFreshness::CachedFallback,
    ));
    let (fragments, next) = current.render_diff_with_snapshot(&accepted);
    assert_eq!(fragments.len(), 1);
    assert!(
        fragments[0]
            .render()
            .starts_with("<repository_observation>")
    );
    assert!(fragments[0].render().ends_with("</repository_observation>"));
    assert!(!fragments[0].render().contains("retained instruction body"));
    assert_eq!(next, current.snapshot());
    assert!(current.render_diff(&next).is_empty());

    let (fragments, refreshed) = previous.render_diff_with_snapshot(&next);
    assert_eq!(fragments.len(), 1);
    assert!(fragments[0].render().contains("refreshed"));
    assert_eq!(refreshed, accepted);
    assert!(previous.render_diff(&refreshed).is_empty());
}

#[test]
fn freshness_change_with_new_instructions_delivers_the_replacement_body() {
    let old = LoadedAgentsMd::from_text_for_testing("old body");
    let mut previous = WorldState::default();
    previous.add_section(AgentsMdState::new_cached(
        Some(&old),
        None,
        AgentsMdFreshness::CachedFallback,
    ));
    let new = LoadedAgentsMd::from_text_for_testing("new body");
    let mut current = WorldState::default();
    current.add_section(AgentsMdState::new(Some(&new)));
    let (fragments, accepted) = current.render_diff_with_snapshot(&previous.snapshot());
    assert_eq!(
        render_fragments(fragments),
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nnew body\n</INSTRUCTIONS>"
        )]
    );
    assert_eq!(accepted, current.snapshot());
    assert!(current.render_diff(&accepted).is_empty());
}

#[test]
fn legacy_snapshot_without_freshness_deserializes_as_cached() {
    let snapshot: AgentsMdSnapshot = serde_json::from_value(json!({
        "text": "legacy cached instructions"
    }))
    .expect("legacy AGENTS.md snapshot");

    assert_eq!(snapshot.freshness, AgentsMdFreshness::CachedFallback);
    let removal = WorldStateSection::render_diff(
        &AgentsMdState::default(),
        PreviousSectionState::Known(&snapshot),
    )
    .expect("an undecodable legacy body must not be mistaken for absent instructions");
    assert!(removal.render().contains(AgentsMdState::REMOVAL_NOTICE));
}

fn render_fragments(fragments: Vec<Box<dyn ContextualUserFragment>>) -> Vec<ResponseItem> {
    fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect()
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn retention_requires_latest_body_scope_and_revocation() {
    let make = |directory: &str| {
        AgentsMdState::from_instructions(
            Some(UserInstructions {
                directory: Some(directory.to_string()),
                text: "same body".to_string(),
            }),
            AgentsMdFreshness::Refreshed,
        )
    };
    let old = make("/old");
    let current = make("/new");
    let old_items = render_fragments(vec![
        WorldStateSection::render_diff(&old, PreviousSectionState::Absent).unwrap(),
    ]);
    assert!(!AgentsMdState::retained_state_supported(
        &current.snapshot(),
        &old_items
    ));
    let new_items = render_fragments(vec![
        WorldStateSection::render_diff(&current, PreviousSectionState::Absent).unwrap(),
    ]);
    assert!(AgentsMdState::retained_state_supported(
        &current.snapshot(),
        &new_items
    ));
    let removed = AgentsMdState::new(None);
    assert!(!AgentsMdState::retained_state_supported(
        &removed.snapshot(),
        &new_items
    ));
    let mut history = new_items;
    history.extend(render_fragments(vec![
        WorldStateSection::render_diff(&removed, PreviousSectionState::Known(&current.snapshot()))
            .unwrap(),
    ]));
    assert!(AgentsMdState::retained_state_supported(
        &removed.snapshot(),
        &history
    ));
    assert!(!AgentsMdState::retained_state_supported(
        &current.snapshot(),
        &history
    ));
}
#[test]
fn survivability_unavailable_observation_does_not_revoke_saved_instructions() {
    let loaded = LoadedAgentsMd::from_text_for_testing("Never edit protected.txt.");
    let accepted = AgentsMdState::new(Some(&loaded)).snapshot();
    for previous in [
        PreviousSectionState::Known(&accepted),
        PreviousSectionState::Unknown,
        PreviousSectionState::Absent,
    ] {
        let missing = AgentsMdState::new_cached(None, None, AgentsMdFreshness::IncompleteRead);
        let notice = missing.render_diff(previous).unwrap();
        assert_eq!(notice.role(), "developer");
        assert!(!notice.render().contains(AgentsMdState::REMOVAL_NOTICE));
        assert!(
            missing
                .render_diff(PreviousSectionState::Known(&missing.snapshot()))
                .is_none(),
            "an unchanged unavailable observation must not grow the prompt each step"
        );
    }
    let mut saved = serde_json::to_value(accepted).unwrap();
    for _ in 0..3 {
        let resumed = AgentsMdState::new_cached(None, None, AgentsMdFreshness::IncompleteRead)
            .with_unavailable_fallback(|| Some(saved.clone()));
        let full = resumed
            .render_diff(PreviousSectionState::Absent)
            .unwrap()
            .render();
        assert!(full.contains("Never edit protected.txt."));
        assert!(!full.contains(AgentsMdState::REMOVAL_NOTICE));
        assert_eq!(
            resumed.snapshot().freshness,
            AgentsMdFreshness::CachedFallback
        );
        saved = serde_json::to_value(resumed.snapshot()).unwrap();
    }
    let verified_empty = AgentsMdState::new_cached(None, None, AgentsMdFreshness::Refreshed)
        .with_unavailable_fallback(|| panic!("successful observation must not consult fallback"));
    assert!(verified_empty.snapshot().text.is_none());
    let current = AgentsMdState::new_cached(Some(&loaded), None, AgentsMdFreshness::CachedFallback)
        .with_unavailable_fallback(|| None);
    assert!(current.snapshot().text.is_some());
}
#[test]
fn audit_instruction_authority_incomplete_nonempty_and_legacy_recovery() {
    let old = LoadedAgentsMd::from_text_for_testing("Never edit protected.txt.");
    let saved = serde_json::to_value(AgentsMdState::new(Some(&old)).snapshot()).unwrap();
    let global = LoadedAgentsMd::from_text_for_testing("Current global correction.");
    let mut previous = saved;
    let mut first = None;
    for _ in 0..3 {
        let state =
            AgentsMdState::new_cached(Some(&global), None, AgentsMdFreshness::IncompleteRead)
                .with_unavailable_fallback(|| Some(previous));
        let fragments = WorldStateSection::render_fragments(&state, PreviousSectionState::Absent);
        assert_eq!(fragments[0].role(), "user");
        assert_eq!(fragments[1].role(), "developer");
        let text = fragments[0].render();
        assert!(
            fragments[1]
                .render()
                .contains("freshness: cached_may_be_stale")
        );
        assert!(text.contains("Never edit protected.txt."));
        assert!(text.contains("Current global correction."));
        if let Some(first) = &first {
            assert_eq!(&text, first);
        } else {
            first = Some(text);
        }
        previous = serde_json::to_value(state.snapshot()).unwrap();
    }
    let direct = AgentsMdState::new_cached(Some(&global), None, AgentsMdFreshness::IncompleteRead);
    let fragments = WorldStateSection::render_fragments(&direct, PreviousSectionState::Absent);
    assert_eq!(fragments[1].role(), "developer");
    assert!(fragments[1].render().contains("freshness: incomplete_read"));
}

#[test]
fn every_unverified_body_delivery_has_a_following_developer_observation() {
    let old = LoadedAgentsMd::from_text_for_testing("old rule");
    let mut previous = WorldState::default();
    previous.add_section(AgentsMdState::new(Some(&old)));
    for freshness in [
        AgentsMdFreshness::IncompleteRead,
        AgentsMdFreshness::CachedFallback,
    ] {
        let current = LoadedAgentsMd::from_text_for_testing("current binding rule");
        let mut world = WorldState::default();
        world.add_section(AgentsMdState::new_cached(Some(&current), None, freshness));
        for (fragments, snapshot) in [
            world.render_full_with_snapshot(),
            world.render_diff_with_snapshot(&previous.snapshot()),
        ] {
            assert_eq!(fragments.len(), 2);
            assert_eq!(fragments[0].role(), "user");
            assert_eq!(fragments[1].role(), "developer");
            assert!(fragments[0].render().contains("current binding rule"));
            assert!(!fragments[1].render().contains("current binding rule"));
            assert!(
                fragments[1]
                    .render()
                    .contains(freshness.model_visible_description())
            );
            let retained = render_fragments(fragments);
            assert!(
                world
                    .render_history_diff(Some(&snapshot), &retained)
                    .is_empty()
            );
            // Losing the warning must not certify the retained body as current.
            let replay = world.render_history_diff(Some(&snapshot), &retained[..1]);
            assert!(replay.iter().any(|fragment| {
                fragment.role() == "developer"
                    && fragment
                        .render()
                        .contains(freshness.model_visible_description())
            }));
            let mut reversed = retained.clone();
            reversed.reverse();
            assert!(
                !world
                    .render_history_diff(Some(&snapshot), &reversed)
                    .is_empty()
            );
        }
    }
}

#[test]
fn legacy_resume_preserves_plain_unversioned_rules_alongside_current_global_text() {
    let saved = json!({"directory": "/original", "text": "Never modify protected.txt."});
    let current = LoadedAgentsMd::from_text_for_testing("New global instructions.");
    let state = AgentsMdState::new_cached(Some(&current), None, AgentsMdFreshness::IncompleteRead)
        .with_unavailable_fallback(|| Some(saved));
    let mut world = WorldState::default();
    world.add_section(state);
    let fragments = world.render_full();
    assert!(
        fragments[0]
            .render()
            .contains("Never modify protected.txt.")
    );
    assert!(fragments[0].render().contains("/original"));
    assert!(fragments[0].render().contains("New global instructions"));
    assert!(
        fragments[0]
            .render()
            .contains("current host/global snapshot supersedes")
    );
    assert_eq!(fragments[1].role(), "developer");
}

#[test]
fn alternating_partial_resume_preserves_legacy_scope_without_nesting_bodies() {
    let mut saved = json!({
        "directory": "/original",
        "text": "Old global policy.\n\nNever modify protected.txt."
    });
    let current = LoadedAgentsMd::from_text_for_testing("Current global correction.");
    let mut first_body = None;
    for index in 0..100 {
        let state = AgentsMdState::new_cached(
            (index % 2 == 0).then_some(&current),
            None,
            AgentsMdFreshness::IncompleteRead,
        )
        .with_unavailable_fallback(|| Some(saved));
        let mut world = WorldState::default();
        world.add_section(state);
        let (fragments, snapshot) = world.render_full_with_snapshot();
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].role(), "user");
        assert_eq!(fragments[1].role(), "developer");
        let body = fragments[0].render();
        assert_eq!(body.matches("Never modify protected.txt.").count(), 1);
        assert!(body.contains("/original"));
        assert!(
            fragments[1]
                .render()
                .contains("current host/global snapshot supersedes")
        );
        if let Some(first) = &first_body {
            assert_eq!(
                &body, first,
                "identical recovery must not nest rendered aggregates"
            );
        } else {
            first_body = Some(body);
        }
        saved = snapshot.section("agents_md").unwrap().clone();
    }
}

#[test]
fn recovered_global_supersession_is_delivered_even_when_body_and_freshness_match() {
    let accepted = AgentsMdState::from_instructions(
        Some(UserInstructions {
            directory: Some("/original".into()),
            text: "Never modify protected.txt.".into(),
        }),
        AgentsMdFreshness::CachedFallback,
    )
    .snapshot();
    let state = AgentsMdState::new_cached(None, None, AgentsMdFreshness::CachedFallback)
        .with_unavailable_fallback(|| Some(serde_json::to_value(&accepted).unwrap()));
    let fragments = state.render_fragments(PreviousSectionState::Known(&accepted));
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].role(), "developer");
    assert!(
        fragments[0]
            .render()
            .contains("current host/global snapshot supersedes")
    );
}

#[test]
fn empty_unavailable_observation_is_reintroduced_after_history_loss() {
    let mut world = WorldState::default();
    world.add_section(AgentsMdState::new_cached(
        None,
        None,
        AgentsMdFreshness::IncompleteRead,
    ));
    let (fragments, snapshot) = world.render_full_with_snapshot();
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].role(), "developer");
    let retained = render_fragments(fragments);
    assert!(
        world
            .render_history_diff(Some(&snapshot), &retained)
            .is_empty()
    );
    let replay = world.render_history_diff(Some(&snapshot), &[]);
    assert_eq!(replay.len(), 1);
    assert!(replay[0].render().contains("observation unavailable"));
}

#[test]
fn body_and_observation_survive_role_merging_and_compaction_projection() {
    let mut history = Vec::new();
    for rule in ["old rule", "changed rule"] {
        let loaded = LoadedAgentsMd::from_text_for_testing(rule);
        let mut world = WorldState::default();
        world.add_section(AgentsMdState::new_cached(
            Some(&loaded),
            None,
            AgentsMdFreshness::CachedFallback,
        ));
        let (fragments, snapshot) = world.render_full_with_snapshot();
        history.extend(crate::context_manager::updates::merge_contextual_fragments(
            fragments,
        ));
        history = crate::stable_context::project_compaction_context(history.into());
        assert_eq!(history.len(), 2);
        let ResponseItem::Message {
            role: body_role,
            content: body,
            ..
        } = &history[0]
        else {
            panic!("body message")
        };
        let ResponseItem::Message {
            role: observation_role,
            content: observation,
            ..
        } = &history[1]
        else {
            panic!("observation message")
        };
        assert_eq!(body_role, "user");
        assert_eq!(observation_role, "developer");
        let ContentItem::InputText { text } = &body[0] else {
            panic!("body text")
        };
        assert!(text.contains(rule));
        let ContentItem::InputText { text } = &observation[0] else {
            panic!("observation text")
        };
        assert!(text.contains("freshness: cached_may_be_stale"));
        assert!(
            world
                .render_history_diff(Some(&snapshot), &history)
                .is_empty()
        );
    }
    let mut refreshed = WorldState::default();
    refreshed.add_section(AgentsMdState::new(Some(
        &LoadedAgentsMd::from_text_for_testing("fresh rule"),
    )));
    history.extend(crate::context_manager::updates::merge_contextual_fragments(
        refreshed.render_full(),
    ));
    let projected = crate::stable_context::project_compaction_context(history.into());
    assert_eq!(
        projected.len(),
        1,
        "a fresh replacement retires the old cached observation"
    );
}

fn numbered_rules(changes: &[(usize, &str)]) -> String {
    let mut lines = (0..300)
        .map(|index| format!("Rule {index}: keep the documented owner for subsystem {index}."))
        .collect::<Vec<_>>();
    for (index, line) in changes {
        lines[*index] = (*line).to_string();
    }
    lines.join("\n")
}

fn agents_world(body: String) -> WorldState {
    let mut world = WorldState::default();
    world.add_section(AgentsMdState::new(Some(
        &LoadedAgentsMd::from_text_for_testing(body),
    )));
    world
}

#[test]
fn mid_turn_change_is_a_bounded_line_diff_until_the_next_user_message() {
    use codex_utils_output_truncation::TruncationPolicy;
    let policy = TruncationPolicy::Bytes(1_000_000);
    let mut history = crate::context_manager::ContextManager::new();
    let (initial, _) = history.update_world_state(&agents_world(numbered_rules(&[])));
    history.record_items(&render_fragments(initial), policy);
    history.record_items(&[user_message("start the task")], policy);

    // One small edit sends the changed lines, not another complete copy.
    let first_changes = [(120, "Rule 120: route owner changes through the registry.")];
    let first = agents_world(numbered_rules(&first_changes));
    let (update, rollout) = history.update_world_state(&first);
    assert!(rollout.is_some(), "the accepted body advances");
    assert_eq!(update.len(), 1);
    assert_eq!(update[0].role(), "user");
    let text = update[0].render();
    assert!(
        text.contains(
            "\n-Rule 120: keep the documented owner for subsystem 120.\n+Rule 120: route owner changes through the registry.\n"
        ),
        "{text}"
    );
    assert!(!text.contains("Rule 200:"), "unchanged lines stay out: {text}");
    assert!(text.len() < 1_024, "{} bytes", text.len());
    history.record_items(&render_fragments(update), policy);
    let (unchanged, rollout) = history.update_world_state(&first);
    assert!(unchanged.is_empty(), "the retained diff carries the current body");
    assert!(rollout.is_none());

    // A later edit in the same turn chains onto the first diff.
    let second_changes = [first_changes[0], (7, "Rule 7: retired.")];
    let second = agents_world(numbered_rules(&second_changes));
    let (update, _) = history.update_world_state(&second);
    assert_eq!(update.len(), 1);
    let text = update[0].render();
    assert!(
        text.contains("\n-Rule 7: keep the documented owner for subsystem 7.\n+Rule 7: retired.\n"),
        "{text}"
    );
    assert!(!text.contains("Rule 120:"), "an earlier diff is not repeated: {text}");
    history.record_items(&render_fragments(update), policy);
    assert!(history.update_world_state(&second).0.is_empty());

    // The next user message gets one complete current copy.
    history.record_items(&[user_message("continue")], policy);
    let (replacement, _) = history.update_world_state(&second);
    assert_eq!(replacement.len(), 1);
    let text = replacement[0].render();
    assert!(text.contains(REPLACEMENT_NOTICE), "{text}");
    assert!(text.contains(&numbered_rules(&second_changes)));
    history.record_items(&render_fragments(replacement), policy);
    assert!(history.update_world_state(&second).0.is_empty());
}

#[test]
fn line_diff_requires_its_retained_base_and_survives_slot_projection() {
    let base = agents_world(numbered_rules(&[]));
    let (fragments, accepted) = base.render_full_with_snapshot();
    let mut retained = crate::context_manager::updates::merge_contextual_fragments(fragments);
    retained.push(user_message("start the task"));
    let changed_rules = numbered_rules(&[(40, "Rule 40: changed.")]);
    let changed = agents_world(changed_rules.clone());
    let (update, next) = changed.render_history_diff_with_snapshot(Some(&accepted), &retained);
    assert_eq!(update.len(), 1);
    assert!(!update[0].render().contains("Rule 200:"));
    let update = crate::context_manager::updates::merge_contextual_fragments(update);
    let mut with_update = retained.clone();
    with_update.extend(update.clone());
    assert!(
        changed
            .render_history_diff(Some(&next), &with_update)
            .is_empty()
    );
    // The diff is not the repository slot: projection keeps the complete
    // base beside it rather than retiring the base in its favour.
    let projected = crate::stable_context::project_compaction_context(with_update.clone().into());
    assert!(
        changed
            .render_history_diff(Some(&next), &projected)
            .is_empty()
    );

    // Without its base, or without the diff, the complete body is resent.
    let mut missing_base = vec![user_message("start the task")];
    missing_base.extend(update);
    for history in [missing_base, retained] {
        let replay = changed.render_history_diff(Some(&next), &history);
        assert_eq!(replay.len(), 1);
        assert!(replay[0].render().contains(&changed_rules));
    }

    // A change to most lines, or to the scope, is a complete replacement.
    let rewritten = agents_world(
        (0..300)
            .map(|index| format!("Rule {index}: rewritten."))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let (replacement, _) = rewritten.render_history_diff_with_snapshot(Some(&next), &with_update);
    assert!(replacement[0].render().contains(REPLACEMENT_NOTICE));
    let moved = AgentsMdState::from_instructions(
        Some(UserInstructions {
            directory: Some("/moved".into()),
            text: changed_rules,
        }),
        AgentsMdFreshness::Refreshed,
    );
    let replacement = WorldStateSection::render_diff(
        &moved,
        PreviousSectionState::Known(&base_section_snapshot(&accepted)),
    )
    .unwrap();
    assert!(replacement.render().contains(REPLACEMENT_NOTICE));
}

fn base_section_snapshot(snapshot: &super::super::WorldStateSnapshot) -> AgentsMdSnapshot {
    serde_json::from_value(snapshot.section("agents_md").unwrap().clone()).unwrap()
}
