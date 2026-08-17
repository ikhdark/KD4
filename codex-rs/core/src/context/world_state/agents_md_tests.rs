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

    let state = AgentsMdState::new_cached(Some(&loaded), &cwd, AgentsMdFreshness::CachedFallback);
    assert_eq!(
        state.snapshot().text.as_deref(),
        Some(
            "Result provenance: cached_observation; freshness: cached_may_be_stale.\n\ncached instructions"
        )
    );
    assert!(loaded.stable_context_bundle(&cwd).reused);
}

#[test]
fn renders_full_state_and_omits_unchanged_state() {
    let loaded = LoadedAgentsMd::from_text_for_testing("use the project formatter");
    let mut state = WorldState::default();
    state.add_section(AgentsMdState::new(Some(&loaded)));

    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n\nuse the project formatter\n</INSTRUCTIONS>",
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
fn changed_and_removed_state_supersedes_previous_instructions() {
    let previous_loaded = LoadedAgentsMd::from_text_for_testing("old instructions");
    let mut previous = WorldState::default();
    previous.add_section(AgentsMdState::new(Some(&previous_loaded)));

    let current_loaded = LoadedAgentsMd::from_text_for_testing("new instructions");
    let mut current = WorldState::default();
    current.add_section(AgentsMdState::new(Some(&current_loaded)));
    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n\nnew instructions\n</INSTRUCTIONS>",
        )],
        render_fragments(current.render_diff(&previous.snapshot())),
    );

    let mut removed = WorldState::default();
    removed.add_section(AgentsMdState::default());
    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nResult provenance: cached_observation; freshness: cached_may_be_stale.\n\nThe previously provided AGENTS.md instructions no longer apply.\n</INSTRUCTIONS>",
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
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nThese AGENTS.md instructions replace all previously provided AGENTS.md instructions.\n\nResult provenance: direct_file_read; freshness: refreshed_for_this_sampling_step.\n\ncurrent instructions\n</INSTRUCTIONS>",
        )],
        render_fragments(vec![
            WorldStateSection::render_diff(&current, PreviousSectionState::Unknown)
                .expect("unknown state should be replaced"),
        ]),
    );

    assert_eq!(
        vec![user_message(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nResult provenance: cached_observation; freshness: cached_may_be_stale.\n\nThe previously provided AGENTS.md instructions no longer apply.\n</INSTRUCTIONS>",
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
    assert!(!snapshot.sections.contains_key("agents_md"));

    let (rendered_again, next_snapshot) = state.render_diff_with_snapshot(&snapshot);
    assert_eq!(rendered_again.len(), 1);
    assert!(
        rendered_again[0]
            .render()
            .contains("[... context truncated ...]")
    );
    assert_eq!(next_snapshot, snapshot);
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
