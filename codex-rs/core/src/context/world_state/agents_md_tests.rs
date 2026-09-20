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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nuse the project formatter\n</INSTRUCTIONS>",
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
            "text": "Result provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n\nuse the project formatter",
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nQuote &lt;/INSTRUCTIONS&gt; & <example>.\n</INSTRUCTIONS>"
        )]
    );
    assert_eq!(
        state.snapshot().into_value(),
        json!({"agents_md": {
            "text": "Result provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n\nQuote </INSTRUCTIONS> & <example>.",
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\n{source}\n</INSTRUCTIONS>"
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nnew instructions\n</INSTRUCTIONS>",
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\ncurrent instructions\n</INSTRUCTIONS>",
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
fn oversized_agents_md_is_admitted_with_truncation() {
    let loaded = LoadedAgentsMd::from_text_for_testing("x".repeat(50_000));
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));

    let (rendered, snapshot) = state.render_full_with_snapshot();
    let rendered = rendered
        .into_iter()
        .map(|fragment| fragment.render())
        .collect::<Vec<_>>();

    assert_eq!(rendered.len(), 1);
    assert!(rendered[0].contains("[... context truncated ...]"));
    assert_ne!(snapshot, state.snapshot());
    let partial =
        super::super::partial_delivery(snapshot.section("agents_md").expect("partial state"))
            .expect("bounded delivery record");
    assert_eq!(partial.rendered, rendered[0]);
    assert_eq!(partial.role, "user");
    assert!(
        partial.rendered.len()
            <= codex_context_fragments::ModelContextBudget::default().remaining_bytes()
    );
    assert!(snapshot.section("agents_md").unwrap().get("text").is_none());

    let (rendered_again, next_snapshot) = state.render_diff_with_snapshot(&snapshot);
    assert!(rendered_again.is_empty());
    assert_eq!(next_snapshot, snapshot);

    let retained = vec![user_message(&rendered[0])];
    let restored =
        serde_json::from_value(snapshot.clone().into_value()).expect("restore partial snapshot");
    assert!(
        state
            .render_history_diff(Some(&restored), &retained)
            .is_empty()
    );
    let replay = state.render_history_diff(Some(&restored), &[]);
    assert_eq!(
        replay
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        rendered
    );

    let mut history = crate::context_manager::ContextManager::new();
    let (fragments, rollout) = history.update_world_state(&state);
    assert_eq!(history.world_state_baseline(), Some(snapshot.clone()));
    assert_eq!(
        rollout,
        Some(codex_protocol::protocol::WorldStateItem::full(
            snapshot.clone().into_value()
        ))
    );
    let items = render_fragments(fragments);
    history.record_items(
        &items,
        codex_utils_output_truncation::TruncationPolicy::Bytes(100_000),
    );
    let (fragments, rollout) = history.update_world_state(&state);
    assert!(fragments.is_empty());
    assert_eq!(rollout, None);
    history.replace(Vec::new());
    let (fragments, rollout) = history.update_world_state(&state);
    assert_eq!(render_fragments(fragments), items);
    assert_eq!(
        rollout,
        Some(codex_protocol::protocol::WorldStateItem::full(
            snapshot.into_value()
        ))
    );
}

#[test]
fn partial_replacement_does_not_suppress_reversion_or_source_changes() {
    let world_state = |text: String| {
        let mut state = WorldState::default();
        state.add_section(AgentsMdState::new(Some(
            &LoadedAgentsMd::from_text_for_testing(text),
        )));
        state
    };
    let a = world_state("instructions A".to_string());
    let (_, accepted) = a.render_full_with_snapshot();
    let b = world_state("B".repeat(50_000));
    let (bounded, partial) = b.render_diff_with_snapshot(&accepted);
    assert!(bounded[0].render().contains(REPLACEMENT_NOTICE));
    assert!(bounded[0].render().contains("[... context truncated ...]"));
    assert_ne!(partial, accepted);
    let (reverted, restored) = a.render_diff_with_snapshot(&partial);
    assert_eq!(
        render_fragments(reverted),
        vec![user_message(
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\ninstructions A\n</INSTRUCTIONS>"
        )]
    );
    assert_eq!(restored, accepted);

    // A change entirely inside the omitted middle still changes source identity.
    let c = world_state(format!(
        "{}changed{}",
        "B".repeat(25_000),
        "B".repeat(25_000)
    ));
    let (changed, next) = c.render_diff_with_snapshot(&partial);
    assert_eq!(changed.len(), 1);
    assert!(changed[0].render().contains("[... context truncated ...]"));
    assert_ne!(next, partial);
}

#[test]
fn partial_instructions_can_be_delivered_in_full_when_budget_is_available() {
    let mut state = WorldState::default();
    state.add_extension_section(codex_extension_api::WorldStateSectionContribution::new(
        "earlier",
        json!(true),
        |previous| match previous {
            codex_extension_api::PreviousWorldStateSection::Absent => {
                Some(codex_extension_api::RenderedWorldStateFragment::new(
                    "developer",
                    ("", ""),
                    "e".repeat(30_000),
                ))
            }
            _ => None,
        },
    ));
    state.add_section(AgentsMdState::new(Some(
        &LoadedAgentsMd::from_text_for_testing("instructions ".repeat(2_000)),
    )));
    let (first, partial) = state.render_full_with_snapshot();
    assert_eq!(first.len(), 2);
    assert!(first[1].render().contains("[... context truncated ...]"));
    let (complete, accepted) = state.render_diff_with_snapshot(&partial);
    assert_eq!(complete.len(), 1);
    assert!(complete[0].render().contains(REPLACEMENT_NOTICE));
    assert!(
        complete[0]
            .render()
            .contains(&"instructions ".repeat(2_000))
    );
    assert!(!complete[0].render().contains("[... context truncated ...]"));
    assert_eq!(accepted, state.snapshot());
    assert!(state.render_diff(&accepted).is_empty());
}

#[test]
fn tighter_budget_preserves_partial_delivery_and_admits_later_sections() {
    let loaded = LoadedAgentsMd::from_text_for_testing("x".repeat(50_000));
    let mut previous = WorldState::default();
    previous.add_section(AgentsMdState::new(Some(&loaded)));
    let (delivered, accepted) = previous.render_full_with_snapshot();
    let retained = render_fragments(delivered);

    let mut current = WorldState::default();
    current.add_extension_section(codex_extension_api::WorldStateSectionContribution::new(
        "earlier",
        json!(true),
        |_| {
            Some(codex_extension_api::RenderedWorldStateFragment::new(
                "developer",
                ("", ""),
                "e".repeat(10_000),
            ))
        },
    ));
    current.add_section(AgentsMdState::new(Some(&loaded)));
    current.add_extension_section(codex_extension_api::WorldStateSectionContribution::new(
        "later",
        json!(true),
        |_| {
            Some(codex_extension_api::RenderedWorldStateFragment::new(
                "developer",
                ("", ""),
                "later guidance",
            ))
        },
    ));
    let (fragments, next) = current.render_history_diff_with_snapshot(Some(&accepted), &retained);
    assert_eq!(
        fragments
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec!["e".repeat(10_000), "later guidance".to_string()]
    );
    assert_eq!(next.section("agents_md"), accepted.section("agents_md"));
    assert_eq!(next.section("earlier"), Some(&json!(true)));
    assert_eq!(next.section("later"), Some(&json!(true)));
}

#[test]
fn partial_delivery_requires_the_delivered_text_and_role_in_history() {
    let loaded = LoadedAgentsMd::from_text_for_testing("x".repeat(50_000));
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));
    let (fragments, accepted) = state.render_full_with_snapshot();
    let delivered = render_fragments(fragments);
    let mut wrong_role = delivered[0].clone();
    if let ResponseItem::Message { role, .. } = &mut wrong_role {
        *role = "assistant".to_string();
    }
    let unrelated = user_message(
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nunrelated instructions\n</INSTRUCTIONS>",
    );
    for retained in [vec![wrong_role], vec![unrelated]] {
        let (fragments, next) = state.render_history_diff_with_snapshot(Some(&accepted), &retained);
        assert_eq!(render_fragments(fragments), delivered);
        assert_eq!(next, accepted);
    }
    let (fragments, next) = state.render_history_diff_with_snapshot(Some(&accepted), &delivered);
    assert!(fragments.is_empty());
    assert_eq!(next, accepted);
}

#[test]
fn partial_delivery_rollout_patches_replay_replacement_and_removal() {
    use crate::context::world_state::WorldStateSnapshot;
    use codex_protocol::protocol::WorldStateItem;
    use codex_utils_output_truncation::TruncationPolicy;

    let mut history = crate::context_manager::ContextManager::new();
    let mut replay = WorldStateSnapshot::default();
    for (index, body) in [
        Some("old body".to_string()),
        Some("x".repeat(50_000)),
        Some("new body".to_string()),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let loaded = body.map(LoadedAgentsMd::from_text_for_testing);
        let mut state = WorldState::default();
        state.add_section(AgentsMdState::new(loaded.as_ref()));
        let (fragments, rollout) = history.update_world_state(&state);
        let items = render_fragments(fragments);
        assert_eq!(items.len(), 1);
        let rollout = rollout.expect("changed delivery must be persisted");
        if index == 0 {
            assert_eq!(rollout, WorldStateItem::full(state.snapshot().into_value()));
            replay = serde_json::from_value(rollout.state).expect("full snapshot");
        } else {
            assert_eq!(rollout, WorldStateItem::patch(rollout.state.clone()));
            replay
                .apply_merge_patch(&rollout.state)
                .expect("replay delivery patch");
        }
        assert_eq!(history.world_state_baseline(), Some(replay.clone()));
        if index == 1 {
            let section = replay.section("agents_md").expect("agents state");
            assert!(section.get("text").is_none());
            let partial = super::super::partial_delivery(section).expect("partial delivery");
            assert_eq!(items, vec![user_message(partial.rendered)]);
            assert!(partial.rendered.contains("[... context truncated ...]"));
        } else {
            assert_eq!(replay, state.snapshot());
            assert!(
                replay
                    .section("agents_md")
                    .unwrap()
                    .get("partial_delivery")
                    .is_none()
            );
        }
        if index == 2 {
            assert_eq!(
                items,
                vec![user_message(
                    "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nnew body\n</INSTRUCTIONS>"
                )]
            );
        } else if index == 3 {
            assert_eq!(
                items,
                vec![user_message(
                    "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThe previously provided AGENTS.md instructions no longer apply.\n</INSTRUCTIONS>"
                )]
            );
        }
        history.record_items(&items, TruncationPolicy::Bytes(100_000));
        let (unchanged, rollout) = history.update_world_state(&state);
        assert!(unchanged.is_empty());
        assert_eq!(rollout, None);
        // A restored baseline must suppress the same content after each persisted transition.
        assert!(state.render_history_diff(Some(&replay), &items).is_empty());
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThe previously provided instruction body is unchanged.\n</INSTRUCTIONS>"
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
            "# AGENTS.md instructions\n\n<AGENTS_MD_OBSERVATION>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n</AGENTS_MD_OBSERVATION>\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nnew body\n</INSTRUCTIONS>"
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
