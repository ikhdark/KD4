use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn audit21_real_hook_policy_is_exact_or_wholly_omitted() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .finish(),
    );
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::ConfigRequirements;
    use codex_config::ConfigRequirementsToml;
    for oversized in [false, true] {
        let (session, turn, events) = make_session_and_context_with_rx().await;
        let instruction = if oversized {
            format!(
                "Perform the action. {} Except when unapproved, do not perform it. {} Proceed.",
                "x".repeat(15_000),
                "y".repeat(45_000)
            )
        } else {
            "Do not modify protected.txt without approval.".to_string()
        };
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("hook.py");
        let payload = serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit", "additionalContext": instruction
        }})
        .to_string();
        std::fs::write(&script, format!("print({payload:?})\n")).unwrap();
        let layer = serde_json::from_value(serde_json::json!({"hooks": {
            "UserPromptSubmit": [{"hooks": [{
                "type": "command",
                "command": format!("python3 \"{}\"", script.display()),
                "commandWindows": format!("python \"{}\"", script.display())
            }]}]
        }}))
        .unwrap();
        let stack = ConfigLayerStack::new(
            vec![ConfigLayerEntry::new(
                ConfigLayerSource::User {
                    file: turn.config.codex_home.join("config.toml"),
                    profile: None,
                },
                layer,
            )],
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .unwrap();
        session
            .services
            .hooks
            .store(Arc::new(codex_hooks::Hooks::new(
                codex_hooks::HooksConfig {
                    feature_enabled: true,
                    bypass_hook_trust: true,
                    config_layer_stack: Some(stack),
                    ..Default::default()
                },
            )));
        let input = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        let started = std::time::Instant::now();
        let outcome = crate::hook_runtime::inspect_pending_input(&session, &turn, &input).await;
        assert!(!outcome.should_stop);
        assert_eq!(
            outcome.additional_contexts.len(),
            1,
            "hook must actually return context"
        );
        crate::hook_runtime::record_additional_contexts(
            &session,
            &turn,
            outcome.additional_contexts,
        )
        .await
        .unwrap();
        let history = session.clone_history().await;
        let text = developer_input_texts(history.raw_items()).join("\n");
        tracing::info!(
            oversized,
            elapsed_us = %started.elapsed().as_micros(),
            "audit21_hook_to_history"
        );
        let mut completed = false;
        while let Ok(event) = events.try_recv() {
            if let EventMsg::HookCompleted(event) = event.msg {
                assert_eq!(
                    event.run.status,
                    codex_protocol::protocol::HookRunStatus::Completed
                );
                completed = true;
            }
        }
        assert!(completed, "real hook command must complete successfully");
        if oversized {
            assert!(text.contains("Hook instructions omitted in full"), "{text}");
            assert!(!text.contains("Perform the action."));
            assert!(!text.contains("Proceed."));
            let path = text
                .lines()
                .find_map(|line| line.strip_prefix("Full hook output saved to: "))
                .unwrap();
            assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), instruction);
        } else {
            assert_eq!(text, instruction);
        }
    }
}

#[tokio::test]
async fn audit21_full_refresh_preserves_typed_retractions() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .finish(),
    );
    for full_refresh in [false, true] {
        let (mut session, mut turn) = make_session_and_context().await;
        let _ = session.features.enable(Feature::Personality);
        turn.model_info.model_messages = None;
        turn.personality = Some(codex_protocol::config_types::Personality::Friendly);
        turn.developer_instructions = Some("stable-before".into());
        turn.collaboration_mode.settings.developer_instructions =
            Some("old collaboration sentinel".into());
        session
            .state
            .lock()
            .await
            .session_configuration
            .collaboration_mode = turn.collaboration_mode.clone();
        let session = Arc::new(session);
        let previous = Arc::new(turn);
        let step = StepContext::for_test(Arc::clone(&previous));
        session
            .record_context_updates_and_set_reference_context_item(&step)
            .await
            .unwrap();
        commit_test_context_baseline(&session, "typed-retractions").await;
        let old_history = session.clone_history().await;
        let old_text = developer_input_texts(old_history.raw_items()).join("\n");
        assert!(old_text.contains("old collaboration sentinel"));
        assert!(old_text.contains("<personality_spec>"));
        let old_personality = crate::context_manager::updates::personality_message_for(
            &previous.model_info,
            codex_protocol::config_types::Personality::Friendly,
        )
        .unwrap();
        assert!(old_text.contains(&old_personality));
        drop(step);
        let mut turn = Arc::try_unwrap(previous).unwrap_or_else(|_| panic!("shared turn"));
        turn.sub_id.push_str("-next");
        turn.personality = Some(codex_protocol::config_types::Personality::None);
        turn.collaboration_mode.settings.developer_instructions = None;
        session
            .state
            .lock()
            .await
            .session_configuration
            .collaboration_mode = turn.collaboration_mode.clone();
        if full_refresh {
            turn.developer_instructions = Some("stable-after".into());
        }
        let step = StepContext::for_test(Arc::new(turn));
        let started = std::time::Instant::now();
        session
            .record_context_updates_and_set_reference_context_item(&step)
            .await
            .unwrap();
        let history = session.clone_history().await;
        let projection = crate::stable_context::project_stable_context(
            history.raw_items().to_vec().into(),
            crate::stable_context::StableContextTarget::Sampling,
        );
        let text = developer_input_texts(&projection.items).join("\n");
        tracing::info!(
            full = full_refresh,
            elapsed_us = %started.elapsed().as_micros(),
            "audit21_context_refresh"
        );
        assert!(
            !text.contains(&old_personality),
            "full={full_refresh}: old personality survived"
        );
        assert_eq!(text.contains("stable-after"), full_refresh);
        assert_eq!(text.contains("stable-before"), !full_refresh);
        assert!(
            !text.contains("old collaboration sentinel"),
            "full={full_refresh}: {text}"
        );
        assert!(
            text.contains("The previous personality no longer applies."),
            "full={full_refresh}: {text}"
        );
        // The recorded reset is required to revoke the old slot. The existing
        // projector consumes collaboration tombstones instead of resending them.
        let recorded = developer_input_texts(history.raw_items()).join("\n");
        assert!(
            recorded.contains(CollaborationModeInstructions::RESET_INSTRUCTIONS),
            "full={full_refresh}: {recorded}"
        );
        assert!(!text.contains(CollaborationModeInstructions::RESET_INSTRUCTIONS));
    }
}

#[tokio::test]
async fn audit21_context_uses_captured_turn_not_future_session_mode() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .finish(),
    );
    let (session, mut turn) = make_session_and_context().await;
    turn.collaboration_mode.settings.developer_instructions =
        Some("captured turn policy sentinel".into());
    session
        .update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(turn.collaboration_mode.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut future = turn.collaboration_mode.clone();
    future.settings.developer_instructions = Some("future turn policy sentinel".into());
    session
        .update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(future),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let started = std::time::Instant::now();
    let items = build_initial_context(&session, &turn).await;
    let text = developer_input_texts(&items).join("\n");
    tracing::info!(
        elapsed_us = %started.elapsed().as_micros(),
        "audit21_captured_context"
    );
    assert!(text.contains("captured turn policy sentinel"), "{text}");
    assert!(!text.contains("future turn policy sentinel"), "{text}");
    assert_eq!(
        session
            .collaboration_mode()
            .await
            .settings
            .developer_instructions
            .as_deref(),
        Some("future turn policy sentinel")
    );
}
