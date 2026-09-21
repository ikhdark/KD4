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
    assert_eq!(
        render_fragments(world_state.render_full()),
        vec![user_message(
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: cached_observation; freshness: cached_may_be_stale.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\ndistinct supplied rendering\n</INSTRUCTIONS>"
        )]
    );
}

#[test]
fn renders_full_state_and_omits_unchanged_state() {
    let loaded = LoadedAgentsMd::from_text_for_testing("use the project formatter");
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));

    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nuse the project formatter\n</INSTRUCTIONS>",
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
            "text": "Result provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n\nuse the project formatter",
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nQuote &lt;/INSTRUCTIONS&gt; & <example>.\n</INSTRUCTIONS>"
        )]
    );
    assert_eq!(
        state.snapshot().into_value(),
        json!({"agents_md": {
            "text": "Result provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n\nQuote </INSTRUCTIONS> & <example>.",
            "freshness": "refreshed"
        }})
    );
    assert_eq!(
        render_fragments(state.render_diff(&state.snapshot())),
        Vec::<ResponseItem>::new()
    );
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\n{source}\n</INSTRUCTIONS>"
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nnew instructions\n</INSTRUCTIONS>",
        )],
        render_fragments(current.render_diff(&previous.snapshot())),
    );

    let mut removed = WorldState::default();
    removed.add_section(AgentsMdState::default());
    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: cached_observation; freshness: cached_may_be_stale.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThe previously provided AGENTS.md instructions no longer apply.\n</INSTRUCTIONS>",
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\ncurrent instructions\n</INSTRUCTIONS>",
        )],
        render_fragments(vec![
            WorldStateSection::render_diff(&current, PreviousSectionState::Unknown)
                .expect("unknown state should be replaced"),
        ]),
    );

    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: cached_observation; freshness: cached_may_be_stale.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThe previously provided AGENTS.md instructions no longer apply.\n</INSTRUCTIONS>",
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
    assert_eq!(restored.len(), 1);
    assert!(restored[0].render().contains("binding instruction"));
}

#[test]
fn oversized_freshness_updates_do_not_repeat_body_and_middle_changes_do() {
    let body = "b".repeat(50_000);
    let loaded = LoadedAgentsMd::from_text_for_testing(body.clone());
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
    assert!(notice[0].render().len() < 1000);
    assert!(!notice[0].render().contains(&body));
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
    assert_eq!(
        render_fragments(fragments),
        vec![user_message(
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: cached_observation; freshness: cached_may_be_stale.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThe previously provided instruction body is unchanged.\n</INSTRUCTIONS>"
        )]
    );
    assert_eq!(next, current.snapshot());
    assert!(current.render_diff(&next).is_empty());

    let (fragments, refreshed) = previous.render_diff_with_snapshot(&next);
    assert_eq!(
        render_fragments(fragments),
        vec![user_message(
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThe previously provided instruction body is unchanged.\n</INSTRUCTIONS>"
        )]
    );
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nnew body\n</INSTRUCTIONS>"
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
