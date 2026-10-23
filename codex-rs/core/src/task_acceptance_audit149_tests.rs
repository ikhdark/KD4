use super::*;

#[test]
fn absent_integration_cannot_satisfy_a_required_integrator_plan() {
    let mut child = audit149_integrated_child();
    let attempt = child.current_attempt.attempt_id;
    assert!(child_integrated(&child, attempt));
    child.assignment.integration_plan = codex_agent_task_store::IntegrationPlan::TypedIntegratorRequired;
    child.assignment.workspace_strategy = codex_agent_task_store::WorkspaceStrategy::Isolated;
    assert!(!child_integrated(&child, attempt));
}

#[tokio::test]
async fn admitted_complete_scope_is_archived_durably_but_reopen_needs_fresh_proof() {
    let store = criterion_store(false).await;
    store.publish_acceptance_evidence(AcceptanceEvidence { turn_id: "task".into(), mutation_revision: 0,
        observations: BTreeMap::from([("check:consumer".into(), validation(false))]) });
    let decision = store.task_closure("task", 0, &AgentTaskCoordinator::default()).await;
    assert_eq!(decision.request_state, RequestState::Complete);
    let saved = std::sync::Mutex::new(Vec::new());
    store.archive_completed_scope(&decision, |item| { saved.lock().unwrap().push(item); std::future::ready(Ok(())) }).await.unwrap();
    let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
    assert!(lineage.acceptance.is_empty());
    let archived = lineage.acceptance_history["consumer"].clone();
    store.record_accepted_input("explain", ["Explain caching; no repository changes"].into_iter()).await;
    assert_eq!(store.task_closure("explain", 0, &AgentTaskCoordinator::default()).await.request_state, RequestState::Unknown);
    let restored = PlanStore::default();
    // Archival persists a host snapshot, not a synthetic update_plan call.
    let saved = saved.into_inner().unwrap();
    let [snapshot] = saved.as_slice() else { panic!("one durable snapshot") };
    let snapshot = crate::plan_store::plan_snapshot_from_item(snapshot).unwrap();
    let restored_lineage = snapshot.full_lineage_for(&snapshot.current_plan, Some(&snapshot.revision)).unwrap();
    assert!(restored.restore_reconstructed(Some(snapshot.current_plan), Some(restored_lineage), None, &[]).await);
    assert!(restored.snapshot_with_lineage().await.unwrap().1.acceptance_history.contains_key("consumer"));
    let input = store.accepted_input.lock().unwrap().as_ref().unwrap().review_revision();
    update(&store, json!({"plan":plan.plan,"expected_revision":crate::plan_store::plan_revision_with_lineage(Some(&plan), &lineage),
        "acceptance_reviewed_revision":input,"acceptance":{"consumer":archived}})).await;
    assert_eq!(store.task_closure("explain", 0, &AgentTaskCoordinator::default()).await.terminal_action, TerminalAction::Continue);
}

#[tokio::test]
async fn partial_scope_and_failed_archival_preserve_active_debt() {
    let store = criterion_store(false).await;
    let partial = store.task_closure("task", 0, &AgentTaskCoordinator::default()).await;
    store.archive_completed_scope(&partial, |_| async { panic!("partial scope must not be archived") }).await.unwrap();
    assert!(store.snapshot_with_lineage().await.unwrap().1.acceptance.contains_key("consumer"));
    store.publish_acceptance_evidence(AcceptanceEvidence { turn_id:"task".into(), mutation_revision:0,
        observations:BTreeMap::from([("check:consumer".into(),validation(false))]) });
    let complete = store.task_closure("task", 0, &AgentTaskCoordinator::default()).await;
    assert!(store.archive_completed_scope(&complete, |_| async { Err(std::io::Error::other("blocked storage")) }).await.is_err());
    assert!(store.snapshot_with_lineage().await.unwrap().1.acceptance.contains_key("consumer"));
    store.record_accepted_input("continue", ["Status? Continue the unfinished work"].into_iter()).await;
    assert_eq!(store.task_closure("continue",0,&AgentTaskCoordinator::default()).await.request_state,RequestState::Invalidated);
}

#[tokio::test]
async fn malformed_inspection_contract_is_rejected_before_immutability_applies() {
    let store = PlanStore::default();
    store.record_accepted_input("inspect", ["Inspect the source"].into_iter()).await;
    let input = AcceptedPlanInput::from_texts("inspect",["Inspect the source"].into_iter());
    for (start,end) in [(5,5),(10,5)] {
        let args = serde_json::from_value(json!({"plan":[],"acceptance_reviewed_revision":input.review_revision(),
            "acceptance":{"read":{"text":"Read source","origin":input.references()[0],
                "requirement":{"kind":"inspection","environment_id":"local","path":"consumer.rs","start":start,"end":end}}}})).unwrap();
        assert!(store.stage_tool(args).await.is_err());
        assert!(store.snapshot().await.is_none());
    }
}

#[tokio::test]
async fn retired_history_does_not_consume_active_criterion_capacity() {
    let store = PlanStore::default();
    store.record_accepted_input("scope",["Retire prior scope and declare the next task"].into_iter()).await;
    let input = AcceptedPlanInput::from_texts("scope",["Retire prior scope and declare the next task"].into_iter());
    let obligation = json!({"text":"Read source","origin":input.references()[0],
        "requirement":{"kind":"inspection","environment_id":"local","path":"consumer.rs","start":0,"end":1},
        "retired_by":input.references()[0],"retirement_reason":"accepted scope change"});
    let retired = (0..128).map(|i|(format!("old{i}"),obligation.clone())).collect::<serde_json::Map<_,_>>();
    update(&store,json!({"plan":[],"acceptance_reviewed_revision":input.review_revision(),"acceptance":retired})).await;
    let (plan,lineage) = store.snapshot_with_lineage().await.unwrap();
    let mut active = obligation;
    active.as_object_mut().unwrap().remove("retired_by");
    active.as_object_mut().unwrap().remove("retirement_reason");
    update(&store,json!({"plan":[],"expected_revision":crate::plan_store::plan_revision_with_lineage(Some(&plan),&lineage),
        "acceptance_reviewed_revision":input.review_revision(),"acceptance":{"new":active}})).await;
    assert_eq!(store.snapshot_with_lineage().await.unwrap().1.acceptance.len(),129);
}

#[tokio::test]
async fn structured_terminal_states_are_distinct_and_legacy_is_unavailable() {
    let store = criterion_store(false).await;
    let partial = store.task_closure("task",0,&AgentTaskCoordinator::default()).await.disposition();
    let unknown = evaluate(None,None,None,0,&BTreeMap::new(),&AcceptanceEvidence::default()).disposition();
    assert_eq!(unknown.request_state,"unknown");
    assert_eq!(partial.request_state,"partial");
    let roundtrip:codex_protocol::protocol::TurnTaskDisposition = serde_json::from_value(json!(partial)).unwrap();
    assert_eq!(roundtrip.schema_version,1);
    assert!(roundtrip.authority.contains("declared_scope_only"));
    let legacy:codex_protocol::protocol::TurnCompletionAssessment = serde_json::from_value(json!({"failedChecks":[],"verificationGaps":[],"advisories":[]})).unwrap();
    assert!(legacy.task_disposition.is_none());
}
