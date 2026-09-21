use super::*;
use crate::context::world_state::WorldState;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

fn items(fragments: Vec<Box<dyn ContextualUserFragment>>) -> Vec<ResponseItem> {
    fragments
        .into_iter()
        .map(|fragment| ResponseItem::Message {
            id: None,
            role: fragment.role().to_string(),
            content: vec![ContentItem::InputText {
                text: fragment.render(),
            }],
            internal_chat_message_metadata_passthrough: None,
            phase: None,
        })
        .collect()
}

#[test]
fn only_latest_delivered_environment_proves_shell_status_clear_and_removal() {
    let environment = EnvironmentState {
        cwd: PathUri::parse("file:///repo").unwrap(),
        shell: Some("bash".into()),
        os: Some("linux".into()),
        status: EnvironmentStatus::Starting,
    };
    let mut current = EnvironmentsState {
        environments: [("remote".to_string(), environment)].into_iter().collect(),
        current_date: Some("2026-09-21".into()),
        ..Default::default()
    };
    let mut previous = None;
    let mut retained = Vec::new();
    for step in 0..6 {
        match step {
            1 => current.environments.get_mut("remote").unwrap().shell = Some("powershell".into()),
            2 => {
                current.environments.get_mut("remote").unwrap().status =
                    EnvironmentStatus::Available
            }
            3 => current.environments.get_mut("remote").unwrap().os = None,
            4 => current.current_date = None,
            5 => current.environments.clear(),
            _ => {}
        }
        let mut world = WorldState::default();
        world.add_section(current.clone());
        let (fragments, snapshot) =
            world.render_history_diff_with_snapshot(previous.as_ref(), &retained);
        assert_eq!(fragments.len(), 1, "transition {step}");
        let delivered = items(fragments);
        if step > 0 {
            let lost_update = world.render_history_diff(Some(&snapshot), &retained);
            assert_eq!(
                lost_update.len(),
                1,
                "older state cannot prove transition {step}"
            );
            assert!(
                lost_update[0]
                    .render()
                    .contains("replaces all previously provided")
            );
        }
        retained.extend(delivered);
        let (unchanged, unchanged_snapshot) =
            world.render_history_diff_with_snapshot(Some(&snapshot), &retained);
        assert!(unchanged.is_empty(), "intact transition {step}");
        assert_eq!(unchanged_snapshot, snapshot);
        previous = Some(snapshot);
    }
}

#[test]
fn optional_subagent_list_is_bounded_without_truncating_execution_facts() {
    let state = EnvironmentsState {
        current_date: Some("2026-09-21".into()),
        ..Default::default()
    }
    .with_subagents("large optional detail ".repeat(10_000));
    let text = state.render();
    assert!(text.len() < 5_000);
    assert!(text.contains("2026-09-21"));
    assert!(text.contains("Additional subagents omitted"));
}

#[test]
fn latest_delivery_stands_alone_after_an_intermediate_clear_is_lost() {
    let make = |shell: Option<&str>, date: &str| {
        let mut world = WorldState::default();
        world.add_section(EnvironmentsState {
            environments: [(
                "remote".to_string(),
                EnvironmentState {
                    cwd: PathUri::parse("file:///repo").unwrap(),
                    status: EnvironmentStatus::Available,
                    shell: shell.map(str::to_string),
                    os: None,
                },
            )]
            .into_iter()
            .collect(),
            current_date: Some(date.to_string()),
            ..Default::default()
        });
        world
    };
    let old = make(Some("bash"), "2026-09-20");
    let (base, old_snapshot) = old.render_full_with_snapshot();
    let base = items(base);
    let cleared = make(None, "2026-09-20");
    let (clear, clear_snapshot) =
        cleared.render_history_diff_with_snapshot(Some(&old_snapshot), &base);
    let mut complete_history = base.clone();
    complete_history.extend(items(clear));
    let latest = make(None, "2026-09-21");
    let (update, snapshot) =
        latest.render_history_diff_with_snapshot(Some(&clear_snapshot), &complete_history);
    let update = items(update);
    let text = super::super::retained_texts(&update, "user")
        .next()
        .unwrap();
    assert!(
        text.contains("omitted fields are unspecified"),
        "the surviving update must clear omitted old shell data"
    );
    let mut lost_clear = base;
    lost_clear.extend(update);
    assert!(
        latest
            .render_history_diff(Some(&snapshot), &lost_clear)
            .is_empty()
    );
}
