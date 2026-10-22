use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_features::Feature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::json;

use super::tests::make_session_and_context;
use crate::session::step_context::StepContext;

struct CatalogContributor(Arc<AtomicUsize>);

impl ContextContributor for CatalogContributor {
    fn world_state_section_ids(&self) -> &'static [&'static str] {
        &["audit130_catalog"]
    }

    fn contribute_world_state<'a>(
        &'a self,
        _input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        let revision = self.0.load(Ordering::SeqCst);
        Box::pin(async move {
            let body = format!("catalog revision {revision}");
            let snapshot = json!({"body": body});
            vec![WorldStateSectionContribution::new(
                "audit130_catalog", snapshot.clone(), move |previous| {
                    if matches!(previous, PreviousWorldStateSection::Known(old) if old == &snapshot) {
                        return None;
                    }
                    Some(RenderedWorldStateFragment::new(
                        "developer", ("<audit130_catalog>", "</audit130_catalog>"), body.clone(),
                    ))
                },
            ).with_budget_fallback(
                json!({"deferred": true}),
                RenderedWorldStateFragment::new(
                    "developer", ("<audit130_catalog>", "</audit130_catalog>"),
                    "Previous catalog is no longer current; replacement deferred.",
                ),
            )]
        })
    }
}

#[tokio::test]
async fn unchanged_desired_state_retries_only_undelivered_catalog() {
    let (mut session, mut turn) = make_session_and_context().await;
    let revision = Arc::new(AtomicUsize::new(1));
    let mut registry = codex_extension_api::ExtensionRegistryBuilder::new();
    registry.prompt_contributor(Arc::new(CatalogContributor(Arc::clone(&revision))));
    session.services.extensions = Arc::new(registry.build());
    Arc::make_mut(&mut turn.config)
        .features
        .enable(Feature::DeferredExecutor)
        .unwrap();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let mut first = StepContext::for_test(Arc::clone(&turn));
    Arc::get_mut(&mut first).unwrap().loaded_agents_md = Some(Arc::new(
        crate::agents_md::LoadedAgentsMd::from_text_for_testing("initial repository instructions"),
    ));
    let first_world = session
        .record_context_updates_and_set_reference_context_item(&first)
        .await
        .unwrap();
    session
        .bind_context_baseline_candidate("initial", "accepted")
        .await;
    assert!(
        session
            .commit_context_baseline_candidate("initial", "accepted")
            .await
            .unwrap()
    );

    revision.store(2, Ordering::SeqCst);
    let required = "R".repeat(41_000);
    let mut next = StepContext::for_test(Arc::clone(&turn));
    Arc::get_mut(&mut next).unwrap().loaded_agents_md = Some(Arc::new(
        crate::agents_md::LoadedAgentsMd::from_text_for_testing(&required),
    ));
    let count = session.clone_history().await.raw_items().len();
    let desired = session
        .record_step_world_state_if_changed(&first_world, &next)
        .await
        .unwrap();
    let history = session.clone_history().await;
    let texts = |items: &[ResponseItem]| {
        items
            .iter()
            .flat_map(|item| match item {
                ResponseItem::Message { content, .. } => content
                    .iter()
                    .filter_map(|part| match part {
                        ContentItem::InputText { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>()
    };
    let delta = texts(&history.raw_items()[count..]).join("\n");
    assert!(
        delta.contains(&required),
        "required instructions must remain whole"
    );
    assert!(delta.contains("Previous catalog is no longer current"));
    assert!(!delta.contains("catalog revision 2"));
    session
        .bind_context_baseline_candidate("pressured", "accepted")
        .await;
    assert!(
        !session
            .commit_context_baseline_candidate("pressured", "rejected")
            .await
            .unwrap()
    );
    assert!(
        session
            .commit_context_baseline_candidate("pressured", "accepted")
            .await
            .unwrap()
    );
    assert_eq!(
        session
            .clone_history()
            .await
            .world_state_baseline()
            .unwrap()
            .section("audit130_catalog"),
        Some(&json!({"deferred": true}))
    );

    // The observation is unchanged, but accepted delivery still owes the replacement.
    let count = session.clone_history().await.raw_items().len();
    let unchanged = session
        .record_step_world_state_if_changed(&desired, &next)
        .await
        .unwrap();
    assert_eq!(unchanged.snapshot(), desired.snapshot());
    let history = session.clone_history().await;
    let delta = texts(&history.raw_items()[count..]).join("\n");
    assert!(delta.contains("catalog revision 2"));
    assert!(!delta.contains(&required));
    assert!(!delta.contains("Previous catalog is no longer current"));
    session
        .bind_context_baseline_candidate("retry", "accepted")
        .await;
    assert!(
        session
            .commit_context_baseline_candidate("retry", "accepted")
            .await
            .unwrap()
    );
    let count = session.clone_history().await.raw_items().len();
    session
        .record_step_world_state_if_changed(&unchanged, &next)
        .await
        .unwrap();
    assert_eq!(session.clone_history().await.raw_items().len(), count);
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );
}
