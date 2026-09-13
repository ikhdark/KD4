use super::*;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn render(state: AppsInstructionsState, previous: PreviousSectionState<'_, bool>) -> Vec<String> {
    state
        .render_diff(previous)
        .into_iter()
        .map(|fragment| fragment.render())
        .collect()
}

#[test]
fn renders_only_when_apps_become_available() {
    let unavailable = AppsInstructionsState::new(/*available*/ false);
    let available = AppsInstructionsState::new(/*available*/ true);
    let false_snapshot = false;
    let true_snapshot = true;

    assert_eq!(
        render(unavailable, PreviousSectionState::Absent),
        Vec::<String>::new()
    );
    assert_eq!(
        render(available, PreviousSectionState::Absent),
        vec![AppsInstructions.render()]
    );
    assert_eq!(
        render(available, PreviousSectionState::Known(&false_snapshot)),
        vec![AppsInstructions.render()]
    );
    assert_eq!(
        render(available, PreviousSectionState::Known(&true_snapshot)),
        Vec::<String>::new()
    );
    assert_eq!(
        render(unavailable, PreviousSectionState::Known(&true_snapshot)),
        vec![AppsInstructionsUnavailable.render()]
    );
}

#[test]
fn renders_revocation_when_apps_become_unavailable() {
    let true_snapshot = true;
    let rendered = render(
        AppsInstructionsState::new(/*available*/ false),
        PreviousSectionState::Known(&true_snapshot),
    );

    assert_eq!(rendered, vec![AppsInstructionsUnavailable.render()]);
}

#[test]
fn unknown_state_reasserts_current_availability() {
    assert_eq!(
        render(
            AppsInstructionsState::new(true),
            PreviousSectionState::Unknown
        ),
        vec![AppsInstructions.render()]
    );
    assert_eq!(
        render(
            AppsInstructionsState::new(false),
            PreviousSectionState::Unknown
        ),
        vec![AppsInstructionsUnavailable.render()]
    );
}

#[test]
fn unknown_history_reasserts_guidance_once_even_after_revocation() {
    for revoked in [false, true] {
        let mut world_state = super::super::WorldState::default();
        world_state.add_section(AppsInstructionsState::new(true));
        let mut history: Vec<ResponseItem> = vec![ContextualUserFragment::into(AppsInstructions)];
        if revoked {
            history.push(ContextualUserFragment::into(AppsInstructionsUnavailable));
        }
        let (fragments, snapshot) = world_state.render_history_diff_with_snapshot(None, &history);
        assert_eq!(
            fragments
                .iter()
                .map(|fragment| fragment.render())
                .collect::<Vec<_>>(),
            vec![AppsInstructions.render()]
        );
        history.extend(
            fragments
                .into_iter()
                .map(ContextualUserFragment::into_boxed_response_item),
        );
        let (fragments, next) =
            world_state.render_history_diff_with_snapshot(Some(&snapshot), &history);
        assert!(fragments.is_empty());
        assert_eq!(next, snapshot);
    }
}

#[test]
fn persisted_guidance_is_restored_only_when_missing_from_history() {
    let mut world_state = super::super::WorldState::default();
    world_state.add_section(AppsInstructionsState::new(/*available*/ true));
    let snapshot = world_state.snapshot();
    let retained: ResponseItem = ContextualUserFragment::into(AppsInstructions);

    assert_eq!(
        world_state
            .render_history_diff(Some(&snapshot), &[])
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec![AppsInstructions.render()]
    );
    assert!(
        world_state
            .render_history_diff(Some(&snapshot), &[retained])
            .is_empty()
    );
}
