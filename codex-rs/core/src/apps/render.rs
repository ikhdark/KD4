#[cfg(test)]
mod tests {
    use crate::context::world_state::AppsInstructionsState;
    use crate::context::world_state::WorldState;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::APPS_INSTRUCTIONS_CLOSE_TAG;
    use codex_protocol::protocol::APPS_INSTRUCTIONS_OPEN_TAG;

    #[test]
    fn omits_apps_guidance_when_world_state_is_unavailable() {
        let mut world_state = WorldState::default();
        world_state.add_section(AppsInstructionsState::new(false));
        let (fragments, _) = world_state.render_history_diff_with_snapshot(None, &[]);
        assert!(fragments.is_empty());
    }

    #[test]
    fn available_apps_world_state_emits_developer_guidance_once() {
        let mut world_state = WorldState::default();
        world_state.add_section(AppsInstructionsState::new(true));
        let (mut fragments, snapshot) = world_state.render_history_diff_with_snapshot(None, &[]);
        assert_eq!(fragments.len(), 1);
        let item = fragments.remove(0).into_boxed_response_item();
        let ResponseItem::Message { role, content, .. } = &item else {
            panic!("apps guidance must be a message");
        };
        assert_eq!(role, "developer");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("apps guidance must contain one text item");
        };
        assert!(text.starts_with(APPS_INSTRUCTIONS_OPEN_TAG));
        assert!(text.contains("## Apps (Connectors)"));
        assert!(text.contains("discoverable through `tool_search`"));
        assert!(!text.contains("tools_search"));
        assert!(text.contains("or clearly matched by the task"));
        assert!(text.ends_with(APPS_INSTRUCTIONS_CLOSE_TAG));
        let (repeated, _) = world_state.render_history_diff_with_snapshot(Some(&snapshot), &[item]);
        assert!(
            repeated.is_empty(),
            "retained guidance must not be injected twice"
        );
    }
}
