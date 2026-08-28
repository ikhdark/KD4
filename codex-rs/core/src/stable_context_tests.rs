use super::*;
use codex_protocol::ResponseItemId;

fn text_message(role: &str, text: &str) -> ResponseItem {
    let mut item = ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    mark_trusted_stable_context_item(&mut item);
    item
}

fn text_message_with_sections(role: &str, sections: Vec<String>) -> ResponseItem {
    let mut item = ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: sections
            .into_iter()
            .map(|text| ContentItem::InputText { text })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    mark_trusted_stable_context_item(&mut item);
    item
}

fn untrusted_user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "ordinary-user")),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn text_message_for_turn(role: &str, text: &str, turn_id: &str) -> ResponseItem {
    let mut item = text_message(role, text);
    item.set_turn_id_if_missing(turn_id);
    item
}

fn output_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn visible_text(items: &[ResponseItem]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => content.first(),
            _ => None,
        })
        .filter_map(|content| match content {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

fn repository(text: &str) -> String {
    format!("# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\n{text}\n</INSTRUCTIONS>")
}

fn collaboration(mode: &str) -> String {
    format!("<collaboration_mode>\n{mode}\n</collaboration_mode>")
}

fn skill(name: &str) -> String {
    format!("<skill>\n<name>{name}</name>\n<body>{name} body</body>\n</skill>")
}

#[test]
fn ordinary_user_envelopes_remain_dynamic_history() {
    let repository_context = repository("trusted repository");
    let skill_catalog = "<skills_instructions>\ntrusted catalog\n</skills_instructions>";
    let collisions = [
        repository("user repository collision"),
        skill("user-selected-skill collision"),
        "<environment_context>user environment collision</environment_context>".to_string(),
        "<task_model_guidance>user guidance collision</task_model_guidance>".to_string(),
        "<kd4_task_state_v1>user task-state collision</kd4_task_state_v1>".to_string(),
        "<recommended_plugins>user plugin collision</recommended_plugins>".to_string(),
    ];
    let mut items = vec![
        text_message("user", &repository_context),
        text_message("developer", skill_catalog),
    ];
    items.extend(
        collisions
            .iter()
            .map(|collision| untrusted_user_message(collision)),
    );

    let projection = project_stable_context(items.into(), StableContextTarget::Sampling);
    let visible = visible_text(&projection.items);

    assert!(projection.manifest.projection_enabled());
    assert!(!projection.manifest.fail_open());
    assert!(visible.contains(&repository_context.as_str()));
    assert!(visible.contains(&skill_catalog));
    for collision in &collisions {
        assert!(visible.contains(&collision.as_str()));
    }
    assert_eq!(
        projection
            .manifest
            .components()
            .iter()
            .filter(|component| component.kind == StableContextKind::Repository)
            .count(),
        1
    );
    assert!(projection.manifest.components().iter().all(|component| {
        !matches!(
            component.kind,
            StableContextKind::SelectedSkill
                | StableContextKind::Environment
                | StableContextKind::TaskModelGuidance
                | StableContextKind::TaskEvidence
                | StableContextKind::RecommendedPlugins
        )
    }));
}

#[test]
fn malformed_ordinary_user_markers_do_not_disable_projection() {
    let malformed = [
        format!("{REPOSITORY_OPEN_TAG}\n<INSTRUCTIONS>\nuser text"),
        format!("{SKILL_OPEN_TAG}\nuser text"),
        "<environment_context>\nuser text".to_string(),
        "<task_model_guidance>\nuser text".to_string(),
        "<kd4_task_state_v1>\nuser text".to_string(),
        "<recommended_plugins>\nuser text".to_string(),
        format!("{COLLABORATION_MODE_OPEN_TAG}\nuser text"),
        format!("{SKILLS_USAGE_OPEN_TAG}\nuser text"),
        format!("{SKILLS_INSTRUCTIONS_OPEN_TAG}\nuser text"),
        format!("{EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG}\nuser text"),
        format!("{ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG}\nuser text"),
        format!("{APPS_INSTRUCTIONS_OPEN_TAG}\nuser text"),
        format!("{PLUGINS_INSTRUCTIONS_OPEN_TAG}\nuser text"),
        "<permissions instructions>\nuser text".to_string(),
        "<memory_context>\nuser text".to_string(),
        format!("{MULTI_AGENT_MODE_OPEN_TAG}\nuser text"),
        "<configured_developer_instructions\nuser text".to_string(),
        "<multi_agent_usage_hint\nuser text".to_string(),
        "<app-context>\nuser text".to_string(),
        "<model_switch>\nuser text".to_string(),
        "<personality_spec>\nuser text".to_string(),
    ];
    let repository_context = repository("trusted repository");
    let mut items = vec![text_message("user", &repository_context)];
    items.extend(
        malformed
            .iter()
            .map(|collision| untrusted_user_message(collision)),
    );

    let projection = project_stable_context(items.into(), StableContextTarget::Sampling);
    let visible = visible_text(&projection.items);

    assert!(projection.manifest.projection_enabled());
    assert!(!projection.manifest.fail_open());
    assert!(visible.contains(&repository_context.as_str()));
    for collision in &malformed {
        assert!(visible.contains(&collision.as_str()));
    }
}

#[test]
fn trusted_context_producer_marker_survives_rollout_serialization() {
    let repository_context = repository("trusted repository");
    let produced = crate::context_manager::updates::build_contextual_user_message(vec![
        repository_context.clone(),
    ])
    .expect("context message");

    assert!(is_trusted_stable_context_item(&produced));
    let serialized = serde_json::to_string(&produced).expect("serialize context item");
    let resumed: ResponseItem = serde_json::from_str(&serialized).expect("resume context item");
    assert!(is_trusted_stable_context_item(&resumed));

    let projection = project_stable_context(vec![resumed].into(), StableContextTarget::Sampling);
    assert_eq!(
        visible_text(&projection.items),
        vec![repository_context.as_str()]
    );
}

#[test]
fn repository_replacement_keeps_only_the_current_variant() {
    let old = repository("old");
    let current = repository("current");
    let projection = project_stable_context(
        vec![
            text_message("user", &old),
            text_message("user", "do work"),
            text_message("user", &current),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let text = visible_text(&projection.items);
    assert!(!text.contains(&old.as_str()));
    assert!(text.contains(&current.as_str()));
    assert!(text.contains(&"do work"));
    assert_eq!(visible_text(&projection.fallback_items), text);
    assert!(projection.manifest.components().iter().any(|component| {
        component.kind == StableContextKind::Repository
            && component.disposition == StableContextDisposition::Replaced
    }));
}

#[test]
fn tagged_root_orchestration_replaces_the_previous_variant() {
    let old = "<root_orchestration_instructions>old root policy</root_orchestration_instructions>";
    let current =
        "<root_orchestration_instructions>current root policy</root_orchestration_instructions>";
    let projection = project_stable_context(
        vec![
            text_message("developer", old),
            text_message("developer", current),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert_eq!(visible_text(&projection.items), vec![current]);
    assert!(projection.manifest.components().iter().any(|component| {
        component.kind == StableContextKind::RootCoordinator
            && component.disposition == StableContextDisposition::Replaced
    }));
}

#[test]
fn skill_catalog_authorities_replace_independently() {
    let host = "<skills_instructions>host catalog</skills_instructions>";
    let old_extension =
        "<extension_skills_instructions>old extension catalog</extension_skills_instructions>";
    let extension =
        "<extension_skills_instructions>extension catalog</extension_skills_instructions>";
    let environment =
        "<environment_skills_instructions>environment catalog</environment_skills_instructions>";
    let projection = project_stable_context(
        vec![
            text_message("developer", host),
            text_message("developer", old_extension),
            text_message("developer", environment),
            text_message("developer", extension),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let visible = visible_text(&projection.items);
    assert_eq!(visible, vec![host, extension, environment]);
    assert!(!visible.contains(&old_extension));
    assert_eq!(
        projection
            .manifest
            .components()
            .iter()
            .filter(|component| component.kind == StableContextKind::SkillCatalog)
            .count(),
        3
    );
}

#[test]
fn selected_skill_compacts_each_catalog_without_collapsing_authority() {
    let host = "<skills_instructions>host catalog</skills_instructions>";
    let extension =
        "<extension_skills_instructions>extension catalog</extension_skills_instructions>";
    let environment =
        "<environment_skills_instructions>environment catalog</environment_skills_instructions>";
    let selected = skill("one");
    let first = project_stable_context(
        vec![
            text_message("developer", host),
            text_message("developer", extension),
            text_message("developer", environment),
            text_message("user", "use one"),
            text_message("user", &selected),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let visible = visible_text(&first.items);
    assert_eq!(
        visible
            .iter()
            .filter(|text| text.contains("active_catalog"))
            .count(),
        3
    );
    assert!(
        visible
            .iter()
            .any(|text| text.starts_with("<skills_instructions>"))
    );
    assert!(
        visible
            .iter()
            .any(|text| text.starts_with("<extension_skills_instructions>"))
    );
    assert!(
        visible
            .iter()
            .any(|text| text.starts_with("<environment_skills_instructions>"))
    );

    let second = project_stable_context(first.items, StableContextTarget::Sampling);
    assert_eq!(
        second
            .manifest
            .components()
            .iter()
            .filter(|component| component.kind == StableContextKind::SkillCatalog)
            .count(),
        3
    );
}

#[test]
fn configured_developer_instructions_are_latest_wins_without_exposing_identity_marker() {
    let old = configured_developer_instructions_sections(Some("old developer policy"));
    let unchanged = configured_developer_instructions_sections(Some("current developer policy"));
    let replayed = configured_developer_instructions_sections(Some("current developer policy"));
    let projection = project_stable_context(
        vec![
            text_message_with_sections("developer", old),
            text_message("developer", "ordinary developer conversation"),
            text_message_with_sections("developer", unchanged),
            text_message_with_sections("developer", replayed),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let text = visible_text(&projection.items);
    assert_eq!(
        text.iter()
            .filter(|text| **text == "current developer policy")
            .count(),
        1
    );
    assert!(!text.contains(&"old developer policy"));
    assert!(text.contains(&"ordinary developer conversation"));
    assert!(
        text.iter()
            .all(|text| !text.contains("configured_developer_instructions"))
    );
}

#[test]
fn configured_developer_instructions_precede_collaboration_in_canonical_prefix() {
    let configured =
        configured_developer_instructions_sections(Some("configured developer policy"));
    let collaboration = collaboration("collaboration mode");
    let projection = project_stable_context(
        vec![
            text_message("developer", &collaboration),
            text_message_with_sections("developer", configured),
            text_message("user", "do work"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert_eq!(
        visible_text(&projection.items),
        vec![
            "configured developer policy",
            collaboration.as_str(),
            "do work"
        ]
    );
}

#[test]
fn multi_agent_usage_hint_removal_drops_the_previous_hint() {
    let old = multi_agent_usage_hint_sections(Some("old delegation guidance"));
    let removed = multi_agent_usage_hint_sections(None);
    let projection = project_stable_context(
        vec![
            text_message_with_sections("developer", old),
            text_message_with_sections("developer", removed),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert!(visible_text(&projection.items).is_empty());
    assert!(projection.manifest.components().iter().any(|component| {
        component.kind == StableContextKind::MultiAgentUsageHint
            && !component.active
            && component.disposition == StableContextDisposition::Removed
    }));
}

#[test]
fn multi_agent_usage_hint_defers_to_the_active_mode() {
    let sections = multi_agent_usage_hint_sections(Some("The spawn tool is available."));

    assert!(sections.iter().any(|section| {
        section.contains("Tool availability does not authorize spawning agents")
            && section.contains("active <multi_agent_mode>")
    }));
}

#[test]
fn repository_removal_removes_the_notice_and_obsolete_instructions() {
    let old = repository("old");
    let removal = repository(REPOSITORY_REMOVAL_NOTICE);
    let projection = project_stable_context(
        vec![text_message("user", &old), text_message("user", &removal)].into(),
        StableContextTarget::Sampling,
    );

    assert!(visible_text(&projection.items).is_empty());
    assert!(projection.manifest.components().iter().any(|component| {
        component.kind == StableContextKind::Repository
            && !component.active
            && component.disposition == StableContextDisposition::Removed
    }));
}

#[test]
fn collaboration_default_plan_default_keeps_only_the_latest_variant() {
    let default_one = collaboration("Default one");
    let plan = collaboration("Plan");
    let default_two = collaboration("Default two");
    let projection = project_stable_context(
        vec![
            text_message("developer", &default_one),
            text_message("developer", &plan),
            text_message("developer", &default_two),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert_eq!(visible_text(&projection.items), vec![default_two.as_str()]);
}

#[test]
fn environment_permissions_are_accounted_separately_and_replace_prior_variant() {
    let old = "<permissions instructions>\nold\n</permissions instructions>";
    let current = "<permissions instructions>\ncurrent\n</permissions instructions>";
    let projection = project_stable_context(
        vec![
            text_message("developer", old),
            text_message("developer", current),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert_eq!(visible_text(&projection.items), vec![current]);
    assert!(projection.manifest.components().iter().any(|component| {
        component.kind == StableContextKind::EnvironmentPermissions
            && component.disposition == StableContextDisposition::Replaced
    }));
}

#[test]
fn selected_skills_gate_catalog_until_the_next_user_turn() {
    let catalog = "<skills_instructions>\nfull catalog\n</skills_instructions>";
    let usage = "<skills_usage_instructions>\nusage\n</skills_usage_instructions>";
    let selected_a = skill("a");
    let selected_b = skill("b");
    let active = project_stable_context(
        vec![
            text_message("developer", usage),
            text_message("developer", catalog),
            text_message("user", "use both"),
            text_message("user", &selected_a),
            text_message("user", &selected_b),
        ]
        .into(),
        StableContextTarget::Sampling,
    );
    let active_text = visible_text(&active.items);
    assert!(!active_text.contains(&usage));
    assert!(
        active_text
            .iter()
            .any(|text| text.contains("active_catalog"))
    );
    assert!(active_text.contains(&selected_a.as_str()));
    assert!(active_text.contains(&selected_b.as_str()));

    let restored = project_stable_context(
        vec![
            text_message("developer", usage),
            text_message("developer", catalog),
            text_message("user", "use both"),
            text_message("user", &selected_a),
            text_message("user", &selected_b),
            text_message("user", "new capability discovery"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );
    let restored_text = visible_text(&restored.items);
    assert!(restored_text.contains(&usage));
    assert!(restored_text.contains(&catalog));
    assert!(!restored_text.contains(&selected_a.as_str()));
    assert!(!restored_text.contains(&selected_b.as_str()));
}

#[test]
fn stronger_role_selected_skills_still_gate_catalog() {
    let catalog = "<skills_instructions>\nfull catalog\n</skills_instructions>";
    let selected = skill("admin-skill");

    for role in ["system", "developer"] {
        let projection = project_stable_context(
            vec![
                text_message("developer", catalog),
                text_message("user", "use the configured skill"),
                text_message(role, &selected),
            ]
            .into(),
            StableContextTarget::Sampling,
        );

        assert!(projection.manifest.components().iter().any(|component| {
            component.kind == StableContextKind::SkillCatalog
                && component.disposition == StableContextDisposition::Gated
        }));
        assert!(
            projection
                .manifest
                .components()
                .iter()
                .any(|component| { component.kind == StableContextKind::SelectedSkill })
        );
        assert!(projection.items.iter().any(|item| {
            matches!(
                item,
                ResponseItem::Message {
                    role: projected_role,
                    content,
                    ..
                } if projected_role == role
                    && content.iter().any(|item| {
                        matches!(item, ContentItem::InputText { text } if text == &selected)
                    })
            )
        }));
    }
}

#[test]
fn selected_skill_change_and_resolution_failure_replace_then_restore_catalog() {
    let catalog = "<skills_instructions>\nfull catalog\n</skills_instructions>";
    let usage = "<skills_usage_instructions>\nusage\n</skills_usage_instructions>";
    let selected_a = skill("a");
    let selected_b = skill("b");
    let changed = project_stable_context(
        vec![
            text_message("developer", usage),
            text_message("developer", catalog),
            text_message("user", "select a"),
            text_message("user", &selected_a),
            text_message("user", "change to b"),
            text_message("user", &selected_b),
        ]
        .into(),
        StableContextTarget::Sampling,
    );
    let changed_text = visible_text(&changed.items);
    assert!(!changed_text.contains(&selected_a.as_str()));
    assert!(changed_text.contains(&selected_b.as_str()));
    assert!(
        changed_text
            .iter()
            .any(|text| text.contains("active_catalog"))
    );

    let unresolved = project_stable_context(
        vec![
            text_message("developer", usage),
            text_message("developer", catalog),
            text_message("user", "select a"),
            text_message("user", &selected_a),
            text_message("user", "select a missing skill"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );
    let unresolved_text = visible_text(&unresolved.items);
    assert!(unresolved_text.contains(&usage));
    assert!(unresolved_text.contains(&catalog));
    assert!(!unresolved_text.contains(&selected_a.as_str()));
}

#[test]
fn malformed_registered_fragment_fails_open() {
    let malformed = "<skills_instructions>\nmissing close";
    let items: Arc<[ResponseItem]> = vec![text_message("developer", malformed)].into();
    let projection = project_stable_context(Arc::clone(&items), StableContextTarget::Sampling);

    assert!(!projection.manifest.projection_enabled());
    assert!(projection.manifest.fail_open());
    assert_eq!(projection.items.as_ref(), items.as_ref());
}

#[test]
fn semantic_repository_identity_changes_manifest_with_identical_bytes() {
    let instructions = repository("same bytes");
    let projection = project_stable_context(
        vec![text_message("user", &instructions)].into(),
        StableContextTarget::Sampling,
    );
    let left = projection
        .manifest
        .with_repository_identity(Some(([1; 32], false, false)));
    let right = projection
        .manifest
        .with_repository_identity(Some(([2; 32], false, true)));

    assert_ne!(left.fingerprint(), right.fingerprint());
    assert!(right.components().iter().any(|component| {
        component.kind == StableContextKind::Repository
            && component.disposition == StableContextDisposition::Replaced
    }));
}

#[test]
fn generic_preparation_target_retains_all_recognized_history() {
    let old = repository("old");
    let current = repository("current");
    let items: Arc<[ResponseItem]> =
        vec![text_message("user", &old), text_message("user", &current)].into();

    let projection = project_stable_context(Arc::clone(&items), StableContextTarget::FailOpen);

    assert_eq!(projection.items.as_ref(), items.as_ref());
    assert!(!projection.manifest.projection_enabled());
    assert!(projection.manifest.fail_open());
    assert!(
        projection.manifest.components().iter().all(|component| {
            component.disposition == StableContextDisposition::RetainedFallback
        })
    );
}

#[test]
fn sampling_projection_ignores_removed_environment_switch() {
    const CHILD_PROCESS: &str = "KD4_TEST_STABLE_CONTEXT_FIXED_BEHAVIOR";
    const REMOVED_ENVIRONMENT_SWITCH: &str = "CODEX_STABLE_CONTEXT_PROJECTION";

    if std::env::var_os(CHILD_PROCESS).is_some() {
        let old = repository("old");
        let current = repository("current");
        let projection = project_stable_context(
            vec![text_message("user", &old), text_message("user", &current)].into(),
            StableContextTarget::Sampling,
        );

        assert!(projection.manifest.projection_enabled());
        assert_eq!(visible_text(&projection.items), vec![current.as_str()]);
        return;
    }

    let output = std::process::Command::new(
        std::env::current_exe().expect("current test executable should be available"),
    )
    .args([
        "sampling_projection_ignores_removed_environment_switch",
        "--nocapture",
    ])
    .env(CHILD_PROCESS, "1")
    .env(REMOVED_ENVIRONMENT_SWITCH, "baseline")
    .output()
    .expect("isolated stable-context test process should run");

    assert!(
        output.status.success(),
        "isolated stable-context test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn mixed_registered_message_is_split_into_canonical_prefix_and_dynamic_history() {
    let old = repository("old");
    let current = repository("current");
    let mut mixed = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText { text: old },
            ContentItem::InputText {
                text: "unregistered material".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    mark_trusted_stable_context_item(&mut mixed);
    let items: Arc<[ResponseItem]> = vec![mixed, text_message("user", &current)].into();

    let projection = project_stable_context(items, StableContextTarget::Sampling);

    assert_eq!(
        visible_text(&projection.items),
        vec![current.as_str(), "unregistered material"]
    );
    assert!(projection.manifest.projection_enabled());
    assert!(!projection.manifest.fail_open());
}

#[test]
fn mixed_stable_and_ordinary_user_message_sets_latest_real_user_boundary() {
    let catalog = "<skills_instructions>\nfull catalog\n</skills_instructions>";
    let usage = "<skills_usage_instructions>\nusage\n</skills_usage_instructions>";
    let selected = skill("a");
    let mut mixed = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: repository("current"),
            },
            ContentItem::InputText {
                text: "new ordinary task".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    mark_trusted_stable_context_item(&mut mixed);
    let projection = project_stable_context(
        vec![
            text_message("developer", usage),
            text_message("developer", catalog),
            text_message("user", "use a"),
            text_message("user", &selected),
            mixed,
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let text = visible_text(&projection.items);
    assert!(text.contains(&usage));
    assert!(text.contains(&catalog));
    assert!(!text.contains(&selected.as_str()));
    assert!(text.contains(&"new ordinary task"));
}

#[test]
fn token_efficiency_places_volatile_context_after_reusable_history_prefix() {
    let repository = repository("stable repository");
    let collaboration = collaboration("stable collaboration");
    let environment = "<environment_context>volatile environment</environment_context>";
    let task_evidence = "<kd4_task_state_v1>volatile task evidence</kd4_task_state_v1>";
    let plugins = "<recommended_plugins>volatile catalog</recommended_plugins>";
    let projection = project_stable_context(
        vec![
            text_message("user", "prior user"),
            output_message("prior answer"),
            text_message("user", environment),
            text_message("developer", &collaboration),
            text_message("user", task_evidence),
            text_message("user", plugins),
            text_message("user", &repository),
            text_message("user", "current task"),
            output_message("current tail"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert_eq!(
        visible_text(&projection.items),
        vec![
            repository.as_str(),
            collaboration.as_str(),
            "prior user",
            "prior answer",
            environment,
            task_evidence,
            plugins,
            "current task",
            "current tail",
        ]
    );
}

#[test]
fn unchanged_runtime_context_preserves_the_previous_request_prefix() {
    let environment = "<environment_context>same environment</environment_context>";
    let guidance = "<task_model_guidance>same guidance</task_model_guidance>";
    let first = project_stable_context(
        vec![
            text_message_for_turn("user", environment, "turn-1"),
            text_message_for_turn("user", guidance, "turn-1"),
            text_message_for_turn("user", "first task", "turn-1"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );
    let second = project_stable_context(
        vec![
            text_message_for_turn("user", environment, "turn-1"),
            text_message_for_turn("user", guidance, "turn-1"),
            text_message_for_turn("user", "first task", "turn-1"),
            text_message_for_turn("user", guidance, "turn-2"),
            text_message_for_turn("user", "second task", "turn-2"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let first_visible = visible_text(&first.items);
    let second_visible = visible_text(&second.items);
    assert_eq!(
        &second_visible[..first_visible.len()],
        first_visible.as_slice()
    );
    assert_eq!(second_visible.last(), Some(&"second task"));
    assert_eq!(
        second_visible
            .iter()
            .filter(|text| **text == guidance)
            .count(),
        1
    );
    assert!(second.manifest.components().iter().any(|component| {
        component.kind == StableContextKind::TaskModelGuidance && component.active
    }));
}

#[test]
fn task_state_updates_keep_only_the_latest_model_visible_snapshot() {
    let first = "<kd4_task_state_v1>\n## Current state\n- first\n</kd4_task_state_v1>";
    let second = "<kd4_task_state_v1>\n## Current state\n- second\n</kd4_task_state_v1>";
    let current = "<kd4_task_state_v1>\n## Current state\n- current\n</kd4_task_state_v1>";
    let projection = project_stable_context(
        vec![
            text_message("user", first),
            output_message("first tool result"),
            text_message("user", second),
            output_message("second tool result"),
            text_message("user", current),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let text = visible_text(&projection.items);
    assert!(!text.contains(&first));
    assert!(!text.contains(&second));
    assert!(text.contains(&current));
    assert!(text.contains(&"first tool result"));
    assert!(text.contains(&"second tool result"));
    assert_eq!(
        text.iter()
            .filter(|text| text.starts_with("<kd4_task_state_v1>"))
            .count(),
        1
    );
    assert!(projection.manifest.components().iter().any(|component| {
        component.kind == StableContextKind::TaskEvidence
            && component.active
            && component.disposition == StableContextDisposition::Replaced
    }));
}

#[test]
fn recommended_plugins_expire_after_the_requesting_turn() {
    let plugins = "<recommended_plugins>requested catalog</recommended_plugins>";
    let projection = project_stable_context(
        vec![
            text_message_for_turn("user", plugins, "turn-1"),
            text_message_for_turn("user", "suggest a plugin", "turn-1"),
            output_message("prior answer"),
            text_message_for_turn("user", "fix the parser", "turn-2"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert!(!visible_text(&projection.items).contains(&plugins));
    let component = projection
        .manifest
        .components()
        .iter()
        .find(|component| component.kind == StableContextKind::RecommendedPlugins)
        .expect("recommended plugin component");
    assert!(!component.active);
    assert_eq!(component.disposition, StableContextDisposition::Gated);
}

#[test]
fn memory_context_has_dedicated_stable_provenance() {
    let memory = "<memory_context>\nremember this\n</memory_context>";
    let projection = project_stable_context(
        vec![
            text_message("developer", memory),
            text_message("user", "current task"),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    assert!(
        projection
            .manifest
            .components()
            .iter()
            .any(|component| { component.kind == StableContextKind::Memory && component.active })
    );
}

#[test]
fn base_and_compact_catalog_identities_are_deterministic_without_local_reuse() {
    let catalog = "<skills_instructions>\nfull catalog\n</skills_instructions>";
    let selected = skill("deterministic");
    let items = vec![
        text_message("developer", catalog),
        text_message("user", "select"),
        text_message("user", &selected),
    ];
    let first = project_stable_context(items.clone().into(), StableContextTarget::Sampling)
        .manifest
        .with_base_model("gpt-test", "stable base");
    let second = project_stable_context(items.into(), StableContextTarget::Sampling)
        .manifest
        .with_base_model("gpt-test", "stable base");

    assert_eq!(first.fingerprint(), second.fingerprint());
    for manifest in [&first, &second] {
        assert!(manifest.components().iter().any(|component| {
            component.kind == StableContextKind::BaseModel && !component.local_reused
        }));
    }
    let catalog = first
        .components()
        .iter()
        .find(|component| component.kind == StableContextKind::SkillCatalog)
        .expect("catalog component");
    assert!(!catalog.identity.semantic_id.contains("task"));
    assert!(!catalog.identity.semantic_id.contains("time"));
}

#[test]
fn collaboration_reset_removes_plan_and_the_reset_notice() {
    let plan = collaboration("Plan");
    let reset = collaboration(COLLABORATION_RESET_NOTICE);
    let projection = project_stable_context(
        vec![
            text_message("developer", &plan),
            text_message("developer", &reset),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let visible = visible_text(&projection.items);
    assert!(!visible.contains(&plan.as_str()));
    assert!(!visible.contains(&reset.as_str()));
}

#[test]
fn repository_reconstruction_keeps_only_current_canonical_variant() {
    let repository_a = repository("old");
    let repository_b = repository("current");
    let projection = project_stable_context(
        vec![
            text_message("user", &repository_a),
            text_message("user", "dynamic history survives"),
            text_message("user", &repository_b),
        ]
        .into(),
        StableContextTarget::Sampling,
    );

    let visible = visible_text(&projection.items);
    assert!(!visible.contains(&repository_a.as_str()));
    assert!(visible.contains(&repository_b.as_str()));
    assert!(visible.contains(&"dynamic history survives"));
}

#[test]
fn runtime_context_variants_are_stable_and_replace_by_semantic_slot() {
    let fragments = [
        ("user", "<environment_context>old</environment_context>"),
        ("user", "<recommended_plugins>old</recommended_plugins>"),
        ("developer", "<app-context>old</app-context>"),
        ("developer", "<model_switch>old</model_switch>"),
        ("developer", "<personality_spec>old</personality_spec>"),
        ("user", "<environment_context>current</environment_context>"),
        ("user", "<recommended_plugins>current</recommended_plugins>"),
        ("developer", "<app-context>current</app-context>"),
        ("developer", "<model_switch>current</model_switch>"),
        ("developer", "<personality_spec>current</personality_spec>"),
    ];
    let items = fragments
        .into_iter()
        .map(|(role, text)| text_message(role, text))
        .collect::<Vec<_>>();

    let first = project_stable_context(items.clone().into(), StableContextTarget::Sampling);
    let second = project_stable_context(items.into(), StableContextTarget::Sampling);

    assert_eq!(first.manifest.fingerprint(), second.manifest.fingerprint());
    let visible = visible_text(&first.items);
    assert!(visible.iter().all(|text| !text.contains("old")));
    assert_eq!(visible.len(), 5);
    for kind in [
        StableContextKind::Environment,
        StableContextKind::RecommendedPlugins,
        StableContextKind::AppContext,
        StableContextKind::ModelSwitch,
        StableContextKind::Personality,
    ] {
        assert!(first.manifest.components().iter().any(|component| {
            component.kind == kind
                && component.active
                && component.disposition == StableContextDisposition::Replaced
        }));
    }
}
