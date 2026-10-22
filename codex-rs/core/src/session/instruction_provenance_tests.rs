use super::*;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn kd4_ip_008_same_slug_personality_text_refreshes_accepted_context() {
    let (session, turn, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.features.enable(Feature::Personality).unwrap();
        },
    )
    .await;
    let mut initial = unwrap_test_arc(turn, "initial personality turn");
    initial.personality = Some(codex_protocol::config_types::Personality::Friendly);
    initial.model_info.model_messages = Some(
        serde_json::from_value(json!({
            "instructions_variables": {"personality_friendly": "catalog style A"}
        }))
        .unwrap(),
    );
    let base = initial.model_info.base_instructions.clone();
    let slug = initial.model_info.slug.clone();
    let turn = Arc::new(initial);
    let step = StepContext::for_test(Arc::clone(&turn));
    session
        .record_context_updates_and_set_reference_context_item(&step)
        .await
        .unwrap();
    commit_test_context_baseline(&session, "style-A").await;
    assert!(
        session
            .prepare_context_update(&step)
            .await
            .context_items()
            .is_empty()
    );
    drop(step);
    let mut next = unwrap_test_arc(turn, "next personality turn");
    next.sub_id.push_str("-next");
    next.model_info
        .model_messages
        .as_mut()
        .unwrap()
        .instructions_variables
        .as_mut()
        .unwrap()
        .personality_friendly = Some("catalog style B".into());
    assert_eq!(next.model_info.slug, slug);
    assert_eq!(next.model_info.base_instructions, base);
    let turn = Arc::new(next);
    let step = StepContext::for_test(Arc::clone(&turn));
    let prepared = session.prepare_context_update(&step).await;
    assert!(
        developer_input_texts(prepared.context_items())
            .iter()
            .any(|text| text.contains("catalog style B"))
    );
    session
        .compare_and_record_context_updates(prepared, session.services.planning_generation())
        .await
        .unwrap()
        .expect("accept current personality");
    commit_test_context_baseline(&session, "style-B").await;
    let history = session.clone_history().await;
    let projected = crate::stable_context::project_stable_context(
        history.raw_items().to_vec().into(),
        crate::stable_context::StableContextTarget::Sampling,
    );
    let texts = developer_input_texts(&projected.items);
    assert!(!texts.iter().any(|text| text.contains("catalog style A")));
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains("catalog style B"))
            .count(),
        1
    );
    assert!(
        session
            .prepare_context_update(&step)
            .await
            .context_items()
            .is_empty()
    );
    drop(step);
    let mut next = unwrap_test_arc(turn, "reset personality turn");
    next.sub_id.push_str("-reset");
    next.personality = Some(codex_protocol::config_types::Personality::None);
    let step = StepContext::for_test(Arc::new(next));
    let prepared = session.prepare_context_update(&step).await;
    assert!(
        developer_input_texts(prepared.context_items())
            .iter()
            .any(|text| { text.contains("The previous personality no longer applies.") })
    );
    session
        .compare_and_record_context_updates(prepared, session.services.planning_generation())
        .await
        .unwrap()
        .expect("accept personality reset");
    commit_test_context_baseline(&session, "style-reset").await;
    assert!(
        session
            .prepare_context_update(&step)
            .await
            .context_items()
            .is_empty()
    );
}

struct DisappearingWorldState {
    phase: Arc<AtomicUsize>,
    role: &'static str,
}

impl codex_extension_api::ContextContributor for DisappearingWorldState {
    fn world_state_section_ids(&self) -> &'static [&'static str] {
        &["runtime_policy", "neighbor_policy"]
    }

    fn contribute_world_state<'a>(
        &'a self,
        _input: codex_extension_api::WorldStateContributionInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<codex_extension_api::WorldStateSectionContribution>,
    > {
        let phase = self.phase.load(Ordering::SeqCst);
        let role = self.role;
        Box::pin(async move {
            if phase == 1 {
                std::future::pending::<()>().await;
            }
            if phase == 4 {
                return Vec::new();
            }
            let section = |id, body: &'static str| {
                codex_extension_api::WorldStateSectionContribution::new(
                    id,
                    json!({"body": body}),
                    move |previous| {
                        if matches!(previous, codex_extension_api::PreviousWorldStateSection::Known(old) if old["body"] == body)
                        {
                            return None;
                        }
                        Some(codex_extension_api::RenderedWorldStateFragment::new(
                            role,
                            ("", ""),
                            body,
                        ))
                    },
                )
            };
            let mut sections = vec![section("neighbor_policy", "policy A")];
            if phase != 2 {
                sections.push(section(
                    "runtime_policy",
                    if phase == 3 { "policy B" } else { "policy A" },
                ));
            }
            sections
        })
    }
}

#[tokio::test(start_paused = true)]
async fn kd4_ip_006_successful_removal_preserves_role_and_timeout_is_not_removal() {
    for role in ["developer", "user"] {
        let (mut session, turn) = make_session_and_context().await;
        let phase = Arc::new(AtomicUsize::new(0));
        let mut registry = codex_extension_api::ExtensionRegistryBuilder::new();
        registry.prompt_contributor(Arc::new(DisappearingWorldState {
            phase: Arc::clone(&phase),
            role,
        }));
        session.services.extensions = Arc::new(registry.build());
        let step = StepContext::for_test(Arc::new(turn));
        let world = session.build_world_state_for_step(&step).await;
        let initial = session
            .build_initial_context_with_world_state_and_mcp(&step.turn, &world, false)
            .await;
        let mut retained = initial.items;
        let accepted = initial.world_state_snapshot;
        session
            .state
            .lock()
            .await
            .history
            .set_world_state_baseline(accepted.clone());

        phase.store(4, Ordering::SeqCst);
        let empty = session.build_world_state_for_step(&step).await;
        let (fragments, snapshot) =
            empty.render_history_diff_with_snapshot(Some(&accepted), &retained);
        assert_eq!(
            fragments
                .iter()
                .filter(|fragment| fragment.render().contains("<world_state_removal>"))
                .count(),
            2
        );
        assert!(snapshot.section("runtime_policy").is_none());
        assert!(snapshot.section("neighbor_policy").is_none());

        phase.store(1, Ordering::SeqCst);
        let unavailable = session.build_world_state_for_step(&step).await;
        let (fragments, snapshot) =
            unavailable.render_history_diff_with_snapshot(Some(&accepted), &retained);
        assert!(
            !fragments
                .iter()
                .any(|fragment| fragment.render().contains("<world_state_removal>"))
        );
        assert_eq!(
            snapshot.section("runtime_policy"),
            accepted.section("runtime_policy")
        );

        phase.store(2, Ordering::SeqCst);
        let removed = session.build_world_state_for_step(&step).await;
        let (fragments, snapshot) =
            removed.render_history_diff_with_snapshot(Some(&accepted), &retained);
        let withdrawals = fragments
            .iter()
            .filter(|fragment| fragment.render().contains("<world_state_removal>"))
            .collect::<Vec<_>>();
        assert_eq!(withdrawals.len(), 1);
        assert_eq!(withdrawals[0].role(), role);
        let withdrawal = withdrawals[0].render();
        assert!(withdrawal.contains("\"section_id\":\"runtime_policy\""));
        assert!(!withdrawal.contains("neighbor_policy"));
        assert!(snapshot.section("runtime_policy").is_none());
        assert!(snapshot.rendered_extension("runtime_policy").is_none());
        assert_eq!(
            snapshot.section("neighbor_policy"),
            accepted.section("neighbor_policy")
        );
        // A rejected/uncommitted prepare must still offer the withdrawal.
        assert!(
            session
                .build_world_state_for_step(&step)
                .await
                .render_history_diff(Some(&accepted), &retained)
                .iter()
                .any(|fragment| fragment.render().contains("<world_state_removal>"))
        );
        retained.extend(fragments.into_iter().map(|fragment| {
            let item = fragment.into_boxed_response_item();
            assert!(
                !crate::context_manager::is_user_turn_boundary(&item),
                "withdrawal is runtime context, not a new instruction turn"
            );
            item
        }));
        session
            .state
            .lock()
            .await
            .history
            .set_world_state_baseline(snapshot.clone());
        assert!(
            session
                .build_world_state_for_step(&step)
                .await
                .render_history_diff(Some(&snapshot), &retained)
                .is_empty()
        );

        phase.store(3, Ordering::SeqCst);
        let reactivated = session.build_world_state_for_step(&step).await;
        let fragments = reactivated.render_history_diff(Some(&snapshot), &retained);
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment.render() == "policy B")
        );
        assert!(
            !fragments
                .iter()
                .any(|fragment| fragment.render().contains("<world_state_removal>"))
        );
    }
}
