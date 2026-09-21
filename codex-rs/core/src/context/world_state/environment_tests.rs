use super::*;
use crate::context::ContextualUserFragment;
use crate::context::world_state::WorldState;
use anyhow::Result;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::permissions::NetworkSandboxPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn renders_full_environment_state() -> Result<()> {
    let context = EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "zsh")?),
            (
                "devbox".to_string(),
                available("file:///workspace", "bash")?,
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let mut world_state = WorldState::default();
    world_state.add_section(context);

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <environments>
    <environment id="devbox">
      <cwd>/workspace</cwd>
      <shell>bash</shell>
    </environment>
    <environment id="laptop">
      <cwd>/repo</cwd>
      <shell>zsh</shell>
    </environment>
  </environments>
</environment_context>"#,
        )],
        render_fragments(world_state.render_full()),
    );
    Ok(())
}

#[test]
fn environment_os_transitions_are_explicit_and_persisted() -> Result<()> {
    let mut previous = EnvironmentsSnapshot::default();
    for (os, expected) in [
        (Some("linux"), "linux"),
        (Some("windows"), "windows"),
        (None, "unknown"),
    ] {
        let mut environment = available("file:///repo", "sh")?;
        environment.os = os.map(str::to_string);
        let state = EnvironmentsState {
            environments: [(LOCAL_ENVIRONMENT_ID.to_string(), environment)]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let rendered =
            WorldStateSection::render_diff(&state, PreviousSectionState::Known(&previous))
                .expect("OS transition must be communicated")
                .render();
        assert!(
            rendered.contains(&format!("<os>{expected}</os>")),
            "{rendered}"
        );
        previous =
            serde_json::from_value(serde_json::to_value(WorldStateSection::snapshot(&state))?)?;
        assert!(
            WorldStateSection::render_diff(&state, PreviousSectionState::Known(&previous))
                .is_none()
        );
    }
    Ok(())
}

#[test]
fn changed_environments_keep_unchanged_environments_in_replacement() -> Result<()> {
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "bash")?),
            ("unchanged".to_string(), available("file:///same", "sh")?),
            ("devbox".to_string(), starting("file:///workspace")?),
            ("old".to_string(), available("file:///old", "sh")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "zsh")?),
            ("unchanged".to_string(), available("file:///same", "sh")?),
            (
                "devbox".to_string(),
                available("file:///workspace", "powershell")?,
            ),
            ("remote".to_string(), starting("file:///remote")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    });

    assert_eq!(
        vec![replacement_message(
            r#"<environment_context>
  <environments>
    <environment id="devbox">
      <cwd>/workspace</cwd>
      <status>available</status>
      <shell>powershell</shell>
    </environment>
    <environment id="laptop">
      <cwd>/repo</cwd>
      <shell>zsh</shell>
    </environment>
    <environment id="old" status="unavailable" />
    <environment id="remote">
      <cwd>/remote</cwd>
      <status>starting</status>
    </environment>
    <environment id="unchanged">
      <cwd>/same</cwd>
      <shell>sh</shell>
    </environment>
  </environments>
</environment_context>"#,
        )],
        render_fragments(current.render_diff(&previous.snapshot())),
    );
    Ok(())
}

#[test]
fn persisted_turn_context_values_render_a_diff() -> Result<()> {
    let environments = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
        current_date: Some("2026-06-19".to_string()),
        timezone: Some("UTC".to_string()),
        network: Some(NetworkContext::new(
            /*enabled*/ true,
            vec!["old.example.com".to_string()],
            vec![],
        )),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        ..environments.clone()
    });
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState {
        current_date: Some("2026-06-20".to_string()),
        timezone: Some("America/Los_Angeles".to_string()),
        network: Some(NetworkContext::new(
            /*enabled*/ true,
            vec!["new.example.com".to_string()],
            vec!["blocked.example.com".to_string()],
        )),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::External {
                network: NetworkSandboxPolicy::Restricted,
            },
            &[],
        )),
        ..environments
    });

    assert_eq!(
        vec![replacement_message(
            r#"<environment_context>
  <cwd>/repo</cwd>
  <shell>zsh</shell>
  <current_date>2026-06-20</current_date>
  <timezone>America/Los_Angeles</timezone>
  <network enabled="true"><allowed>new.example.com</allowed><denied>blocked.example.com</denied></network>
  <filesystem><permission_profile type="external"><file_system type="external" /></permission_profile></filesystem>
</environment_context>"#,
        )],
        render_fragments(current.render_diff(&previous.snapshot())),
    );
    Ok(())
}

#[test]
fn subagent_only_changes_render_a_diff() {
    let no_subagents = EnvironmentsState {
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        network: Some(NetworkContext::new(
            true,
            vec!["example.com".to_string()],
            vec![],
        )),
        ..Default::default()
    };
    let previous = WorldStateSection::snapshot(&no_subagents);
    let atlas = no_subagents
        .clone()
        .with_subagents("- agent-1: atlas".to_string());

    assert_eq!(
        Some(replacement_message(
            r#"<environment_context>
  <network enabled="true"><allowed>example.com</allowed></network>
  <filesystem><permission_profile type="disabled"><file_system type="unrestricted" /></permission_profile></filesystem>
  <subagents>
    - agent-1: atlas
  </subagents>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &atlas,
            PreviousSectionState::Known(&previous),
        )),
    );

    let previous = WorldStateSection::snapshot(&atlas);
    let nova = no_subagents
        .clone()
        .with_subagents("- agent-2: nova".to_string());
    assert_eq!(
        Some(replacement_message(
            r#"<environment_context>
  <network enabled="true"><allowed>example.com</allowed></network>
  <filesystem><permission_profile type="disabled"><file_system type="unrestricted" /></permission_profile></filesystem>
  <subagents>
    - agent-2: nova
  </subagents>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &nova,
            PreviousSectionState::Known(&previous),
        )),
    );

    let previous = WorldStateSection::snapshot(&nova);
    assert_eq!(
        Some(replacement_message(
            r#"<environment_context>
  <network enabled="true"><allowed>example.com</allowed></network>
  <filesystem><permission_profile type="disabled"><file_system type="unrestricted" /></permission_profile></filesystem>
  <subagents>
    none
  </subagents>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &no_subagents,
            PreviousSectionState::Known(&previous),
        )),
    );
}

#[test]
fn persisted_snapshot_uses_model_visible_path_and_context_values() -> Result<()> {
    let mut world_state = WorldState::default();
    world_state.add_section(EnvironmentsState {
        environments: [(
            "remote".to_string(),
            available("file:///C:/windows", "powershell")?,
        )]
        .into_iter()
        .collect(),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        ..Default::default()
    });

    assert_eq!(
        serde_json::to_value(world_state.snapshot())?,
        json!({
            "environments": {
                "environments": {
                    "remote": {
                        "cwd": "C:\\windows",
                        "status": "available",
                        "shell": "powershell"
                    }
                },
                "filesystem": "<filesystem><permission_profile type=\"disabled\"><file_system type=\"unrestricted\" /></permission_profile></filesystem>"
            }
        }),
    );
    Ok(())
}

#[test]
fn single_environment_diff_reports_newly_known_shell() -> Result<()> {
    let previous = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            EnvironmentState {
                cwd: PathUri::parse("file:///repo")?,
                status: EnvironmentStatus::Available,
                shell: None,
                os: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let current = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let previous = WorldStateSection::snapshot(&previous);

    assert_eq!(
        Some(replacement_message(
            "<environment_context>\n  <cwd>/repo</cwd>\n  <shell>zsh</shell>\n</environment_context>"
        )),
        render_fragment(WorldStateSection::render_diff(
            &current,
            PreviousSectionState::Known(&previous),
        ))
    );
    Ok(())
}

#[test]
fn removed_legacy_environment_renders_unavailable() -> Result<()> {
    let previous = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "bash")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let previous = WorldStateSection::snapshot(&previous);

    assert_eq!(
        Some(replacement_message(
            r#"<environment_context>
  <environments>
    <environment id="local" status="unavailable" />
  </environments>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &EnvironmentsState::default(),
            PreviousSectionState::Known(&previous),
        )),
    );
    Ok(())
}

#[test]
fn known_unknown_and_changed_shell_are_communicated_before_snapshot_advances() -> Result<()> {
    let make_state = |shell: Option<&str>| -> Result<WorldState> {
        let mut state = WorldState::default();
        state.add_section(EnvironmentsState {
            environments: [(
                LOCAL_ENVIRONMENT_ID.to_string(),
                EnvironmentState {
                    cwd: PathUri::parse("file:///repo")?,
                    status: EnvironmentStatus::Available,
                    shell: shell.map(str::to_string),
                    os: None,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        });
        Ok(state)
    };
    let mut history = crate::context_manager::ContextManager::new();
    for (shell, expected) in [
        (Some("bash"), "bash"),
        (None, "unknown"),
        (Some("powershell"), "powershell"),
    ] {
        let state = make_state(shell)?;
        let expected_message = if history.world_state_baseline().is_some() {
            replacement_message
        } else {
            user_message
        };
        let (fragments, item) = history.update_world_state(&state);
        let rendered = render_fragments(fragments);
        assert_eq!(
            rendered,
            vec![expected_message(&format!(
                "<environment_context>\n  <cwd>/repo</cwd>\n  <shell>{expected}</shell>\n</environment_context>"
            ))]
        );
        assert!(item.is_some());
        assert_delivered_snapshot(history.world_state_baseline().unwrap(), state.snapshot());
        history.record_items(
            &rendered,
            codex_utils_output_truncation::TruncationPolicy::Bytes(100_000),
        );
        let (unchanged, item) = history.update_world_state(&state);
        assert!(unchanged.is_empty());
        assert_eq!(item, None);
    }
    Ok(())
}

#[test]
fn removed_global_values_are_explicitly_cleared() {
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
        current_date: Some("2026-09-13".to_string()),
        timezone: Some("UTC".to_string()),
        network: Some(NetworkContext::new(true, vec![], vec![])),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        subagents: Some("agent-1".to_string()),
        ..Default::default()
    });
    let (_, accepted) = previous.render_full_with_snapshot();
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState::default());
    let (fragments, cleared) = current.render_diff_with_snapshot(&accepted);
    assert_eq!(
        render_fragments(fragments),
        vec![replacement_message(
            "<environment_context>\n  <current_date>unknown</current_date>\n  <timezone>unknown</timezone>\n  <network status=\"unspecified\" />\n  <filesystem status=\"unspecified\" />\n  <subagents>\n    none\n  </subagents>\n</environment_context>"
        )]
    );
    assert_delivered_snapshot(cleared.clone(), current.snapshot());
    assert!(current.render_diff(&cleared).is_empty());
}

#[test]
fn unknown_environment_snapshot_is_authoritatively_replaced() {
    let mut state = WorldState::default();
    state.add_section(EnvironmentsState::default());
    let malformed =
        serde_json::from_value(json!({"environments": "unreadable"})).expect("snapshot");
    let (fragments, accepted) = state.render_diff_with_snapshot(&malformed);
    assert_eq!(
        render_fragments(fragments),
        vec![user_message(
            "<environment_context>\n  This environment context replaces all previously provided environment context. Unlisted environments are unavailable; omitted fields are unspecified; omitted subagents means none.\n</environment_context>"
        )]
    );
    assert_delivered_snapshot(accepted.clone(), state.snapshot());
    assert!(state.render_diff(&accepted).is_empty());

    let legacy = user_message(
        "<environment_context>\n  <cwd>/old</cwd>\n  <subagents>old-agent</subagents>\n</environment_context>",
    );
    let (restored, snapshot) = state.render_history_diff_with_snapshot(None, &[legacy]);
    assert_eq!(
        render_fragments(restored),
        vec![user_message(
            "<environment_context>\n  This environment context replaces all previously provided environment context. Unlisted environments are unavailable; omitted fields are unspecified; omitted subagents means none.\n</environment_context>"
        )]
    );
    assert_eq!(snapshot, accepted);
}

#[test]
fn clearing_each_global_field_retains_other_fields_and_persists_delivery() {
    let populated = EnvironmentsState {
        current_date: Some("2026-09-13".to_string()),
        timezone: Some("UTC".to_string()),
        network: Some(NetworkContext::new(true, vec![], vec![])),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        subagents: Some("agent-1".to_string()),
        ..Default::default()
    };
    for (field, expected) in [
        ("current_date", "  <current_date>unknown</current_date>"),
        ("timezone", "  <timezone>unknown</timezone>"),
        ("network", "  <network status=\"unspecified\" />"),
        ("filesystem", "  <filesystem status=\"unspecified\" />"),
        ("subagents", "  <subagents>\n    none\n  </subagents>"),
    ] {
        let mut history = crate::context_manager::ContextManager::new();
        let mut previous = WorldState::default();
        previous.add_section(populated.clone());
        let (fragments, _) = history.update_world_state(&previous);
        history.record_items(
            &render_fragments(fragments),
            codex_utils_output_truncation::TruncationPolicy::Bytes(100_000),
        );
        let mut cleared = populated.clone();
        match field {
            "current_date" => cleared.current_date = None,
            "timezone" => cleared.timezone = None,
            "network" => cleared.network = None,
            "filesystem" => cleared.filesystem = None,
            "subagents" => cleared.subagents = None,
            _ => unreachable!(),
        }
        let mut current = WorldState::default();
        current.add_section(cleared);
        let (fragments, rollout) = history.update_world_state(&current);
        let retained = [
            ("current_date", "  <current_date>2026-09-13</current_date>"),
            ("timezone", "  <timezone>UTC</timezone>"),
            ("network", "  <network enabled=\"true\"></network>"),
            (
                "filesystem",
                "  <filesystem><permission_profile type=\"disabled\"><file_system type=\"unrestricted\" /></permission_profile></filesystem>",
            ),
            ("subagents", "  <subagents>\n    agent-1\n  </subagents>"),
        ];
        let expected_body = retained
            .into_iter()
            .map(|(name, value)| if name == field { expected } else { value })
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_fragments(fragments);
        assert_eq!(
            rendered,
            vec![replacement_message(&format!(
                "<environment_context>\n{expected_body}\n</environment_context>"
            ))]
        );
        assert!(rollout.is_some(), "clear and delivery receipt must persist");
        assert_delivered_snapshot(history.world_state_baseline().unwrap(), current.snapshot());
        history.record_items(
            &rendered,
            codex_utils_output_truncation::TruncationPolicy::Bytes(100_000),
        );
        let (unchanged, rollout) = history.update_world_state(&current);
        assert!(unchanged.is_empty(), "{field} clear should be sent once");
        assert_eq!(rollout, None);
    }
}

#[test]
fn environment_ids_are_escaped_in_current_and_removed_entries() -> Result<()> {
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
        environments: [("old\"&".to_string(), available("file:///old", "bash")?)]
            .into_iter()
            .collect(),
        ..Default::default()
    });
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState {
        environments: [("new\"&".to_string(), available("file:///new", "bash")?)]
            .into_iter()
            .collect(),
        ..Default::default()
    });
    assert_eq!(
        render_fragments(current.render_diff(&previous.snapshot())),
        vec![replacement_message(
            "<environment_context>\n  <environments>\n    <environment id=\"new&quot;&amp;\">\n      <cwd>/new</cwd>\n      <shell>bash</shell>\n    </environment>\n    <environment id=\"old&quot;&amp;\" status=\"unavailable\" />\n  </environments>\n</environment_context>"
        )]
    );
    Ok(())
}

#[test]
fn environment_text_keeps_quotes_and_escapes_markup() -> Result<()> {
    let mut world_state = WorldState::default();
    world_state.add_section(EnvironmentsState {
        environments: [(
            "local".to_string(),
            available("file:///repo", "shell \"quoted\" 'value' <tag>&")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    let snapshot = world_state.snapshot();
    assert_eq!(
        render_fragments(world_state.render_full()),
        vec![user_message(
            "<environment_context>\n  <cwd>/repo</cwd>\n  <shell>shell \"quoted\" 'value' &lt;tag&gt;&amp;</shell>\n</environment_context>"
        )]
    );
    assert!(world_state.render_diff(&snapshot).is_empty());
    Ok(())
}

fn available(cwd: &str, shell: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Available,
        shell: Some(shell.to_string()),
        os: None,
    })
}

#[test]
fn environment_os_survives_snapshot_and_reports_changes() -> Result<()> {
    let mut state = EnvironmentsState {
        environments: [("remote".into(), available("file:///repo", "bash")?)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let previous = state.snapshot();
    state.environments.get_mut("remote").unwrap().os = Some("linux<&".into());
    assert_eq!(
        render_fragment(state.render_diff(PreviousSectionState::Known(&previous))),
        Some(replacement_message(
            "<environment_context>\n  <cwd>/repo</cwd>\n  <os>linux&lt;&amp;</os>\n  <shell>bash</shell>\n</environment_context>"
        ))
    );
    let saved: EnvironmentsSnapshot =
        serde_json::from_value(serde_json::to_value(state.snapshot())?)?;
    assert!(
        state
            .render_diff(PreviousSectionState::Known(&saved))
            .is_none()
    );
    let legacy: EnvironmentsSnapshot = serde_json::from_value(
        json!({"environments":{"remote":{"cwd":"/repo","status":"available","shell":"bash"}},"current_date":null,"timezone":null,"network":null,"filesystem":null,"subagents":null}),
    )?;
    assert!(
        state
            .render_diff(PreviousSectionState::Known(&legacy))
            .is_some()
    );
    Ok(())
}

fn starting(cwd: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Starting,
        shell: None,
        os: None,
    })
}

fn render_fragments(fragments: Vec<Box<dyn ContextualUserFragment>>) -> Vec<ResponseItem> {
    fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect()
}

fn render_fragment(fragment: Option<Box<dyn ContextualUserFragment>>) -> Option<ResponseItem> {
    fragment.map(ContextualUserFragment::into_boxed_response_item)
}

fn replacement_message(text: &str) -> ResponseItem {
    user_message(&text.replacen(
        "<environment_context>\n",
        "<environment_context>\n  This environment context replaces all previously provided environment context. Unlisted environments are unavailable; omitted fields are unspecified; omitted subagents means none.\n",
        1,
    ))
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

fn assert_delivered_snapshot(
    actual: crate::context::world_state::WorldStateSnapshot,
    expected: crate::context::world_state::WorldStateSnapshot,
) {
    let mut actual = serde_json::to_value(actual).unwrap();
    let receipt = actual["environments"]
        .as_object_mut()
        .unwrap()
        .remove("_retained_delivery")
        .expect("admitted environment must record its exact rendered delivery");
    assert_eq!(receipt.as_str().unwrap().len(), 64);
    assert_eq!(actual, serde_json::to_value(expected).unwrap());
}
