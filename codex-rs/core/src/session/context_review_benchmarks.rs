//! Opt-in source-path probes, not model-compliance or provider-cache benchmarks.

use super::make_session_and_context;
use super::user_message;
use crate::agents_md::AgentsMdFreshness;
use crate::agents_md::LoadedAgentsMd;
use crate::context::ContextualUserFragment;
use crate::context::SkillInjection;
use crate::context::world_state::WorldState;
use crate::context_manager::ContextManager;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::stable_context::StableContextTarget;
use crate::stable_context::filter_unchanged_stable_context_items;
use crate::stable_context::mark_trusted_stable_context_item;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_features::Feature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::AcceptedAttemptProvenance;
use codex_protocol::protocol::SkillScope;
use codex_protocol::protocol::TurnContextProvenance;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::model_token_count;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

const POLICY: TruncationPolicy = TruncationPolicy::Bytes(200_000);
const CATALOG_ID: &str = "context_review_catalog";
const CATALOG_MARKER: &str = "BENCH_CATALOG_RULE";

fn fixture_text(bytes: usize) -> String {
    let mut text = String::new();
    while text.len() < bytes {
        text.push_str(
            "Preserve user edits; check the observable result before reporting success.\n",
        );
    }
    text.truncate(bytes);
    text
}

fn trusted_skill(skill: &SkillInjection) -> ResponseItem {
    let mut item = ContextualUserFragment::into(skill.clone());
    mark_trusted_stable_context_item(&mut item);
    item
}

// IDs identify occurrences, not content. Require the core-owned namespace while
// comparing authority and complete rendered content independently of the UUID.
fn same_trusted_body(old: &ResponseItem, item: &ResponseItem) -> bool {
    match (old, item) {
        (
            ResponseItem::Message {
                id: old_id,
                role: old_role,
                content: old_content,
                ..
            },
            ResponseItem::Message {
                id, role, content, ..
            },
        ) => {
            old_id
                .as_ref()
                .is_some_and(|id| id.as_str().starts_with("msg_sctx_"))
                && id
                    .as_ref()
                    .is_some_and(|id| id.as_str().starts_with("msg_sctx_"))
                && old_role == role
                && old_content == content
        }
        _ => false,
    }
}

// Exercise production filtering; only the historical non-reuse benchmark arm bypasses it.
fn reuse_skill(history: &[ResponseItem], skill: &SkillInjection) -> (ResponseItem, bool) {
    let item = trusted_skill(skill);
    let mut filtered = filter_unchanged_stable_context_items(history, vec![item.clone()]);
    assert_eq!(filtered.len(), 1);
    let selected = filtered.pop().unwrap();
    let reused = !same_trusted_body(&item, &selected);
    (selected, reused)
}

fn skill_sequence(skill: &SkillInjection, turns: usize, reuse: bool) -> (Vec<ResponseItem>, usize) {
    let mut history = ContextManager::new();
    let mut prior_request = Vec::new();
    let mut references = 0;
    for turn in 0..turns {
        let (item, referenced) = if reuse {
            reuse_skill(history.raw_items(), skill)
        } else {
            (trusted_skill(skill), false)
        };
        references += usize::from(referenced);
        let injection = vec![item];
        assert_eq!(injection.len(), 1, "activation must survive every turn");
        history.record_items(&injection, POLICY);
        history.record_items(
            &[user_message(&format!("Apply this skill, step {turn}."))],
            POLICY,
        );
        let prepared = history
            .clone()
            .prepare_for_sampling_prompt(&[InputModality::Text], StableContextTarget::Sampling);
        let request = prepared.items().to_vec();
        assert!(
            request.starts_with(&prior_request),
            "do not rewrite the reusable prefix"
        );
        prior_request = request;
    }
    (prior_request, references)
}

fn visible_tokens(items: &[ResponseItem]) -> usize {
    items
        .iter()
        .map(|item| match item {
            ResponseItem::Message { content, .. } => content
                .iter()
                .map(|part| match part {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        model_token_count(text)
                    }
                    _ => 0,
                })
                .sum::<usize>(),
            _ => 0,
        })
        .sum()
}

fn timing(mut samples: Vec<f64>) -> serde_json::Value {
    samples.sort_by(f64::total_cmp);
    json!({"samples": samples.len(), "p50_us": samples[samples.len() / 2],
        "p95_us": samples[(samples.len() - 1) * 95 / 100]})
}

#[expect(clippy::print_stdout, reason = "emits benchmark measurements")]
fn report(name: &str, value: &serde_json::Value) {
    println!("{name}: {}", serde_json::to_string_pretty(value).unwrap());
    if let Some(directory) = std::env::var_os("CODEX_CONTEXT_REVIEW_BENCH_OUTPUT") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{name}.json")),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
}

#[test]
#[ignore = "targeted context-review payload and local-CPU benchmark"]
fn repeated_skill_benchmark() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SKILL.md");
    std::fs::write(&path, fixture_text(8_000)).unwrap();
    let skill = SkillInjection {
        name: "context-review".into(),
        path: path.to_string_lossy().into_owned(),
        contents: std::fs::read_to_string(&path).unwrap(),
        scope: SkillScope::User,
    };
    let original = trusted_skill(&skill);
    assert!(!reuse_skill(&[], &skill).1);
    assert!(reuse_skill(std::slice::from_ref(&original), &skill).1);
    let mut untrusted = original.clone();
    if let ResponseItem::Message { id, .. } = &mut untrusted {
        *id = None;
    }
    assert!(!reuse_skill(&[untrusted], &skill).1);
    let other = SkillInjection {
        path: "different/SKILL.md".into(),
        ..skill.clone()
    };
    assert!(!reuse_skill(std::slice::from_ref(&original), &other).1);
    let other_authority = SkillInjection {
        scope: SkillScope::Admin,
        ..skill.clone()
    };
    assert!(!reuse_skill(std::slice::from_ref(&original), &other_authority).1);
    let mut changed = skill.contents.clone();
    changed.replace_range(3_900..3_940, "CHANGED: never publish this task's work.");
    std::fs::write(&path, &changed).unwrap();
    let changed_skill = SkillInjection {
        contents: std::fs::read_to_string(&path).unwrap(),
        ..skill.clone()
    };
    assert!(!reuse_skill(std::slice::from_ref(&original), &changed_skill).1);
    let (mut retained, _) = skill_sequence(&skill, 5, true);
    retained.retain(|item| !same_trusted_body(item, &original));
    let mut compacted = ContextManager::new();
    compacted.replace(retained);
    assert!(
        !reuse_skill(compacted.raw_items(), &skill).1,
        "references cannot prove body retention"
    );

    let mut cases = Vec::new();
    for turns in [5, 20] {
        for reuse in [false, true] {
            std::hint::black_box(skill_sequence(&skill, turns, reuse));
        }
        let mut baseline_times = Vec::new();
        let mut candidate_times = Vec::new();
        for repetition in 0..31 {
            for reuse in [repetition % 2 == 0, repetition % 2 != 0] {
                let start = Instant::now();
                std::hint::black_box(skill_sequence(&skill, turns, reuse));
                let micros = start.elapsed().as_secs_f64() * 1_000_000.0;
                if reuse {
                    candidate_times.push(micros);
                } else {
                    baseline_times.push(micros);
                }
            }
        }
        let (baseline, _) = skill_sequence(&skill, turns, false);
        let (candidate, references) = skill_sequence(&skill, turns, true);
        assert_eq!(references, turns - 1);
        assert_eq!(
            baseline
                .iter()
                .filter(|item| same_trusted_body(item, &original))
                .count(),
            turns
        );
        assert_eq!(
            candidate
                .iter()
                .filter(|item| same_trusted_body(item, &original))
                .count(),
            1
        );
        assert!(visible_tokens(&candidate) < visible_tokens(&baseline));
        cases.push(json!({"turns": turns, "skill_source_bytes": skill.contents.len(),
            "baseline": {"full_bodies": turns, "serialized_input_bytes": serde_json::to_vec(&baseline).unwrap().len(),
                "visible_text_o200k_tokens": visible_tokens(&baseline), "assembly": timing(baseline_times)},
            "candidate": {"full_bodies": 1, "references": references,
                "serialized_input_bytes": serde_json::to_vec(&candidate).unwrap().len(),
                "visible_text_o200k_tokens": visible_tokens(&candidate), "assembly": timing(candidate_times)}}));
    }
    report(
        "repeated_skill",
        &json!({"profile": "dev", "cases": cases,
        "correctness": ["every turn activated", "unchanged prefix", "changed middle reloaded", "different source and authority not reused", "untrusted quote rejected", "lost full body reloaded"],
        "limits": "Production skill reuse versus historical full-body injection; no model calls, provider cache measurements, file-I/O timing, or task-quality claims."}),
    );
}

struct CatalogContributor {
    body: String,
    calls: Arc<AtomicUsize>,
}

impl ContextContributor for CatalogContributor {
    fn world_state_section_ids(&self) -> &'static [&'static str] {
        &[CATALOG_ID]
    }

    fn contribute_world_state<'a>(
        &'a self,
        _input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let body = self.body.clone();
        Box::pin(async move {
            let snapshot = json!({"body": body});
            let retained = body.clone();
            vec![
                WorldStateSectionContribution::new(CATALOG_ID, snapshot.clone(), move |previous| {
                    if matches!(previous, PreviousWorldStateSection::Known(old) if old == &snapshot)
                    {
                        return None;
                    }
                    Some(RenderedWorldStateFragment::new(
                        "developer",
                        ("<bench_catalog>", "</bench_catalog>"),
                        body.clone(),
                    ))
                })
                .with_retained_fragment_matcher(move |role, text| {
                    role == "developer" && text.contains(&retained)
                }),
            ]
        })
    }
}

fn has_catalog(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::Message { content, .. } if content.iter().any(|part|
        matches!(part, ContentItem::InputText { text } if text.contains(CATALOG_MARKER))))
}

async fn world_fixture(
    catalog_bytes: usize,
) -> (
    Arc<Session>,
    Arc<StepContext>,
    Arc<WorldState>,
    Arc<AtomicUsize>,
) {
    let (mut session, mut turn) = make_session_and_context().await;
    let config = Arc::make_mut(&mut turn.config);
    config.features.enable(Feature::DeferredExecutor).unwrap();
    config.features.disable(Feature::TokenBudget).unwrap();
    config.include_environment_context = false;
    config.include_apps_instructions = false;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.prompt_contributor(Arc::new(CatalogContributor {
        body: format!(
            "{CATALOG_MARKER}{}",
            fixture_text(catalog_bytes - CATALOG_MARKER.len())
        ),
        calls: Arc::clone(&calls),
    }));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let mut step = StepContext::for_test(Arc::new(turn));
    let directory = tempfile::tempdir().unwrap();
    let agents_path = directory.path().join("AGENTS.md");
    std::fs::write(&agents_path, fixture_text(33_000)).unwrap();
    let step_mut = Arc::make_mut(&mut step);
    step_mut.loaded_agents_md = Some(Arc::new(LoadedAgentsMd::from_text_for_testing(
        std::fs::read_to_string(agents_path).unwrap(),
    )));
    step_mut.agents_md_stable_context = None;
    step_mut.agents_md_freshness = AgentsMdFreshness::Refreshed;
    let world = Arc::new(session.build_world_state_for_step(&step).await);
    let (fragments, accepted) = world.render_full_with_snapshot();
    assert!(
        accepted.section(CATALOG_ID).is_none(),
        "first request must defer the catalog"
    );
    let items = fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    assert!(!items.iter().any(has_catalog));
    assert!(items.iter().any(|item| matches!(item, ResponseItem::Message { content, .. }
        if content.iter().any(|part| matches!(part, ContentItem::InputText { text } if text.contains(&fixture_text(33_000)))))));
    let mut reference = step.turn.to_turn_context_item_async().await;
    reference.context_provenance = Some(TurnContextProvenance {
        accepted_attempt: AcceptedAttemptProvenance {
            sampling_request_id: "bench-request".into(),
            physical_attempt_id: "bench-attempt".into(),
        },
        fragment_digests: Vec::new(),
    });
    {
        let mut state = session.state.lock().await;
        state.history.record_items(&items, POLICY);
        state.history.set_world_state_baseline(accepted);
        state.set_reference_context_item(Some(reference));
    }
    (session, step, world, calls)
}

async fn accept_fixture_delivery(session: &Session) {
    let mut state = session.state.lock().await;
    let pending = state
        .pending_context_baseline()
        .expect("delivery must be staged");
    state
        .history
        .set_world_state_baseline(pending.world_state_snapshot);
    state.clear_pending_context_baseline();
}

#[tokio::test]
#[ignore = "targeted context-review session delivery benchmark"]
async fn deferred_world_state_benchmark() {
    let (session, step, world, calls) = world_fixture(7_500).await;
    let initial = session.clone_history().await.raw_items().to_vec();
    let mut baseline_times = Vec::new();
    for _ in 0..31 {
        let start = Instant::now();
        // Historical shortcut for comparison; the production recorder no longer
        // equates desired-state equality with successful delivery.
        let observed = session.build_world_state_for_step(&step).await;
        assert_eq!(observed.snapshot(), world.snapshot());
        baseline_times.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    assert_eq!(
        session.clone_history().await.raw_items(),
        initial.as_slice()
    );
    let baseline_calls = calls.load(Ordering::Relaxed) - 1;

    let start = Instant::now();
    session
        .record_step_world_state_if_changed(&world, &step)
        .await
        .unwrap();
    let candidate_delivery_us = start.elapsed().as_secs_f64() * 1_000_000.0;
    let delivered = session.clone_history().await.raw_items().to_vec();
    assert!(delivered.starts_with(&initial));
    assert_eq!(delivered[initial.len()..].len(), 1);
    assert!(has_catalog(&delivered[initial.len()]));
    accept_fixture_delivery(&session).await;
    let mut candidate_noop_times = Vec::new();
    for _ in 0..31 {
        let start = Instant::now();
        session
            .record_step_world_state_if_changed(&world, &step)
            .await
            .unwrap();
        candidate_noop_times.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    assert_eq!(
        session.clone_history().await.raw_items(),
        delivered.as_slice()
    );
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );

    {
        let mut state = session.state.lock().await;
        let accepted = state.history.world_state_baseline().unwrap();
        let reference = state.reference_context_item();
        state.history.replace(initial.clone());
        state.history.set_world_state_baseline(accepted);
        state.set_reference_context_item(reference);
    }
    session
        .record_step_world_state_if_changed(&world, &step)
        .await
        .unwrap();
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .iter()
            .filter(|item| has_catalog(item))
            .count(),
        1
    );
    accept_fixture_delivery(&session).await;

    let (oversized, oversized_step, oversized_world, _) = world_fixture(50_000).await;
    let before = oversized.clone_history().await.raw_items().to_vec();
    let mut oversized_times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        oversized
            .record_step_world_state_if_changed(&oversized_world, &oversized_step)
            .await
            .unwrap();
        oversized_times.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    assert_eq!(
        oversized.clone_history().await.raw_items(),
        before.as_slice()
    );
    assert!(
        oversized
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );
    report(
        "deferred_world_state",
        &json!({"profile": "dev", "required_body_bytes": 33_000,
        "catalog_body_bytes": 7_500, "baseline": {"steps": 31, "catalog_deliveries": 0,
            "contributor_calls": baseline_calls, "step": timing(baseline_times)},
        "candidate": {"catalog_deliveries": 1, "delivery_us": candidate_delivery_us,
            "following_noop_step": timing(candidate_noop_times), "lost_catalog_restored": true,
            "permanently_oversized_step": timing(oversized_times)},
        "correctness": ["required body retained", "catalog delivered once", "accepted no-op silent", "lost catalog restored", "oversized catalog neither delivered nor acknowledged"],
        "limits": "In-process session with synthetic extension catalog and fixture admission; no physical model requests or model-driven rediscovery measured."}),
    );
}

#[test]
fn skill_reuse_preserves_complete_instructions_and_rejects_stale_or_partial_bodies() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SKILL.md");
    std::fs::write(&path, fixture_text(8_000)).unwrap();
    let mut skill = SkillInjection {
        name: "retained skill".into(),
        path: path.to_string_lossy().into_owned(),
        contents: std::fs::read_to_string(&path).unwrap(),
        scope: SkillScope::User,
    };
    let original = trusted_skill(&skill);
    let (reference, reused) = reuse_skill(std::slice::from_ref(&original), &skill);
    assert!(reused);
    assert!(!crate::context_manager::is_user_turn_boundary(&reference));
    let reference_json = serde_json::to_string(&reference).unwrap();
    assert!(reference_json.contains("<skill_body_ref sha256="));
    assert!(reference_json.contains("Selected again for this turn"));
    assert!(reference_json.len() < serde_json::to_string(&original).unwrap().len() / 4);
    let history = vec![original.clone(), reference.clone()];
    assert!(
        reuse_skill(&history, &skill).1,
        "references must not form chains"
    );
    let mut extension_body = original.clone();
    if let ResponseItem::Message { content, .. } = &mut extension_body {
        let ContentItem::InputText { text } = &mut content[0] else {
            unreachable!()
        };
        *text = text.replace("<scope>user</scope>\n", "");
    }
    let extension_repeat = filter_unchanged_stable_context_items(
        std::slice::from_ref(&extension_body),
        vec![extension_body.clone()],
    );
    assert_eq!(extension_repeat.len(), 1);
    assert!(
        serde_json::to_string(&extension_repeat[0])
            .unwrap()
            .contains("<skill_body_ref sha256=")
    );
    let (restored, reused) = reuse_skill(&[reference], &skill);
    assert!(!reused);
    assert!(same_trusted_body(&restored, &original));

    let mut quote = original.clone();
    if let ResponseItem::Message { id, .. } = &mut quote {
        *id = None;
    }
    assert!(!reuse_skill(&[quote], &skill).1);
    for different in [
        SkillInjection {
            path: "other/SKILL.md".into(),
            ..skill.clone()
        },
        SkillInjection {
            scope: SkillScope::Admin,
            ..skill.clone()
        },
    ] {
        let (delivered, reused) = reuse_skill(&history, &different);
        assert!(!reused);
        assert!(same_trusted_body(&delivered, &trusted_skill(&different)));
    }

    let previous_contents = skill.contents.clone();
    skill
        .contents
        .replace_range(3_000..3_030, "CHANGED_REQUIREMENT: retain files.");
    std::fs::write(&path, &skill.contents).unwrap();
    skill.contents = std::fs::read_to_string(&path).unwrap();
    let (changed, reused) = reuse_skill(&history, &skill);
    assert!(!reused);
    assert!(same_trusted_body(&changed, &trusted_skill(&skill)));
    let history = vec![original, changed];
    skill.contents = previous_contents;
    let (reverted, reused) = reuse_skill(&history, &skill);
    assert!(
        !reused,
        "do not refer past a newer version of the same source"
    );
    assert!(same_trusted_body(&reverted, &trusted_skill(&skill)));

    for body in [
        format!(
            "{}[... context truncated ...]{}",
            fixture_text(1_000),
            fixture_text(1_000)
        ),
        format!(
            "{}[This skill's instructions are incomplete. The omitted portion has not been loaded.]",
            fixture_text(2_000)
        ),
        format!("Instructions were not loaded. {}", fixture_text(2_000)),
        "Short complete instructions.".to_string(),
    ] {
        let partial = SkillInjection {
            contents: body,
            ..skill.clone()
        };
        let prior = trusted_skill(&partial);
        let (delivered, reused) = reuse_skill(std::slice::from_ref(&prior), &partial);
        assert!(
            !reused,
            "partial or shorter-than-reference content must stay intact"
        );
        assert!(same_trusted_body(&delivered, &prior));
    }
}

#[test]
fn skill_references_keep_their_backing_body_during_compaction_projection() {
    let skill = SkillInjection {
        name: "admin skill".into(),
        path: "admin/SKILL.md".into(),
        contents: fixture_text(4_000),
        scope: SkillScope::Admin,
    };
    let mut body = trusted_skill(&skill);
    body.set_turn_id_if_missing("first");
    let (mut reference, reused) = reuse_skill(std::slice::from_ref(&body), &skill);
    assert!(reused);
    reference.set_turn_id_if_missing("second");
    let mut request = user_message("Use the selected skill");
    request.set_turn_id_if_missing("second");
    let input = vec![body.clone(), reference.clone(), request];
    let projected =
        crate::stable_context::project_stable_context(input.into(), StableContextTarget::Sampling);
    assert!(
        projected
            .items
            .iter()
            .any(|item| same_trusted_body(item, &body))
    );
    assert!(
        projected
            .items
            .iter()
            .any(|item| same_trusted_body(item, &reference))
    );
    let compacted =
        crate::compact::strip_compaction_startup_envelopes(Arc::clone(&projected.items));
    assert!(compacted.iter().any(|item| same_trusted_body(item, &body)));
    assert!(reuse_skill(&compacted, &skill).1);

    let mut unrelated = user_message("Now perform an unrelated task");
    unrelated.set_turn_id_if_missing("third");
    let mut next = projected.items.to_vec();
    next.push(unrelated);
    let next =
        crate::stable_context::project_stable_context(next.into(), StableContextTarget::Sampling);
    assert!(!next.items.iter().any(|item| same_trusted_body(item, &body)));
    assert!(
        !next
            .items
            .iter()
            .any(|item| same_trusted_body(item, &reference))
    );
}

#[tokio::test]
async fn world_state_retries_withheld_sections_without_duplicate_staged_delivery() {
    let (session, step, world, _) = world_fixture(7_500).await;
    let initial = session.clone_history().await.raw_items().to_vec();
    let current = session
        .record_step_world_state_if_changed(&world, &step)
        .await
        .unwrap();
    let delivered = session.clone_history().await.raw_items().to_vec();
    assert!(delivered.starts_with(&initial));
    assert_eq!(delivered.len(), initial.len() + 1);
    assert!(has_catalog(&delivered[initial.len()]));
    assert!(
        session
            .state
            .lock()
            .await
            .history
            .world_state_baseline()
            .unwrap()
            .section(CATALOG_ID)
            .is_none()
    );
    session
        .record_step_world_state_if_changed(&current, &step)
        .await
        .unwrap();
    assert_eq!(
        session.clone_history().await.raw_items(),
        delivered.as_slice()
    );
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_some()
    );

    // Losing a staged fragment must not lose the full pending rollout receipt.
    {
        let mut state = session.state.lock().await;
        let accepted = state.history.world_state_baseline().unwrap();
        let reference = state.reference_context_item();
        state.history.replace(initial);
        state.history.set_world_state_baseline(accepted);
        state.set_reference_context_item(reference);
    }
    session
        .record_step_world_state_if_changed(&current, &step)
        .await
        .unwrap();
    let pending = session
        .state
        .lock()
        .await
        .pending_context_baseline()
        .unwrap();
    assert_eq!(
        pending.world_state_item.unwrap().state,
        pending.world_state_snapshot.clone().into_value()
    );
    assert!(pending.world_state_snapshot.section(CATALOG_ID).is_some());
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .iter()
            .filter(|item| has_catalog(item))
            .count(),
        1
    );
    accept_fixture_delivery(&session).await;
    let accepted = session.clone_history().await.raw_items().to_vec();
    session
        .record_step_world_state_if_changed(&current, &step)
        .await
        .unwrap();
    assert_eq!(
        session.clone_history().await.raw_items(),
        accepted.as_slice()
    );
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );
}

#[tokio::test]
async fn world_state_does_not_acknowledge_an_oversized_optional_section() {
    let (session, step, world, _) = world_fixture(50_000).await;
    let before = session.clone_history().await.raw_items().to_vec();
    for _ in 0..2 {
        session
            .record_step_world_state_if_changed(&world, &step)
            .await
            .unwrap();
        assert_eq!(session.clone_history().await.raw_items(), before.as_slice());
        let state = session.state.lock().await;
        assert!(state.pending_context_baseline().is_none());
        assert!(
            state
                .history
                .world_state_baseline()
                .unwrap()
                .section(CATALOG_ID)
                .is_none()
        );
    }
}
