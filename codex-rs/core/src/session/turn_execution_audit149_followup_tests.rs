use super::*;
use crate::plan_store::acceptance::{AcceptanceEvidence, AcceptanceObligation, EvidenceRequirement, RequestState, TaskClosureDecision};
use serde_json::json;

fn source(path: &str, hash: &str, start: u64, end: u64) -> Value {
    json!({"source":"read_file","scope":{"environment":"local","path":path},
        "identity":{"sha256":hash,"ranges":[[start,end]],"values":[]}})
}

#[test]
fn source_registry_precedes_contract_and_recovery_preserves_snapshot_revision() {
    let mut control = TurnExecutionControl::new();
    control.observe_delivered_coverage(&source("consumer.rs","hash",0,5),Some("artifact"),0);
    assert_eq!(control.acceptance_reads.len(),1,"reads before a plan are retained");
    let recovered = json!({"source":"artifact","scope":"artifact","identity":{"sha256":"hash","ranges":[[5,10]],"values":[]}});
    control.observe_delivered_coverage(&recovered,None,0);
    assert_eq!(control.acceptance_reads.values().next().unwrap().3.ranges,vec![(0,10)]);
    control.observe_delivered_coverage(&source("consumer.rs","changed",0,5),None,1);
    control.observe_delivered_coverage(&recovered,None,1);
    let current = control.acceptance_reads.values().next().unwrap();
    assert_eq!(current.0,1);
    assert_eq!(current.3.ranges,vec![(0,5)],"old artifacts cannot fill a changed snapshot");
}

#[test]
fn bounded_source_retention_never_evicts_the_newly_requested_snapshot() {
    let mut control = TurnExecutionControl::new();
    for index in 0..514 {
        control.observe_delivered_coverage(&source(&format!("file{index}.rs"),&format!("hash{index}"),0,1),None,0);
    }
    control.observe_delivered_coverage(&source("requested.rs","requested-hash",0,1),None,0);
    assert_eq!(control.acceptance_reads.len(),512);
    assert!(control.acceptance_reads.values().any(|(_,_,path,_)|path=="requested.rs"));
}

#[test]
fn post_patch_reads_use_their_own_revision_and_validated_replay_is_nonnovel() {
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    let mut read = SamplingToolOutcome::plain(0,SamplingToolOutcomeKind::Success,None);
    read.source_evidence = Some(source("consumer.rs","hash",0,10));
    read.validation_mutation_revision = Some(1);
    collector.state.lock().unwrap().outcomes.push(read.clone());
    let settled = SamplingRequestSettledState { mutation_revision:1,attributed_mutation_revision:0,tool_exposure_revision:0 };
    control.observe_progress(&baseline,&collector,&settled);
    assert_eq!(control.acceptance_reads.values().next().unwrap().0,1);
    let baseline = control.baselines(2);
    let collector = control.collector(&baseline);
    read.validation_mutation_revision = Some(2);
    { let mut state = collector.state.lock().unwrap(); state.outcomes.push(read); state.replayed_ordinals.insert(0); }
    let progress = control.observe_progress(&baseline,&collector,&SamplingRequestSettledState { mutation_revision:2,attributed_mutation_revision:0,tool_exposure_revision:0 });
    assert_eq!(control.acceptance_reads.values().next().unwrap().0,2);
    assert!(progress.is_empty(),"freshness does not mint novel work");
}

#[test]
fn scoped_blockers_and_satisfied_bindings_never_erase_unmapped_work() {
    let mut readiness = CompletionReadiness { mutation_revision:0,
        live_work:vec!["live validation".into()],plan_work:vec!["mapped".into(),"unmapped".into()],
        plan_requirement_ids:vec!["mapped".into(),"unmapped".into()],
        plan_acceptance_ids:BTreeMap::from([("mapped".into(),vec!["criterion".into()])]),
        validation_work:vec!["independent failed test".into()],validation_check_ids:vec!["test".into()],..Default::default() };
    assert_eq!(readiness.remaining(true,&["external".into()]).len(),4);
    let decision = crate::plan_store::acceptance::evaluate(Some("plan".into()),Some("input".into()),Some("input"),0,&BTreeMap::new(),&AcceptanceEvidence::default());
    let complete = TaskClosureDecision { request_state:RequestState::Complete,..decision };
    readiness.reconcile_acceptance(&complete);
    assert_eq!(readiness.plan_work,vec!["unmapped"]);
    assert_eq!(readiness.remaining(true,&["external".into()]).len(),3);
}

#[test]
fn cosmetic_plan_revisions_do_not_renew_acceptance_repairs() {
    let mut control = TurnExecutionControl::new();
    let mut decision = crate::plan_store::acceptance::evaluate(None,Some("input".into()),Some("input"),0,&BTreeMap::new(),&AcceptanceEvidence::default());
    decision.request_state = RequestState::Partial;
    decision.unmet_obligations = vec!["required".into()];
    assert!(control.admit_acceptance_repair(&decision));
    for revision in ["explanation-a","explanation-b","status-only"] {
        decision.plan_revision = Some(revision.into());
        assert!(!control.admit_acceptance_repair(&decision));
    }
    decision.input_revision = Some("new accepted input".into());
    assert!(control.admit_acceptance_repair(&decision));
}

#[tokio::test]
async fn final_named_source_fence_retracts_external_drift_without_advancing_counter() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("consumer.rs");
    std::fs::write(&path,"original").unwrap();
    let (_,mut turn) = crate::session::tests::make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
    let environment = step.environments.primary().unwrap().environment_id.clone();
    let mut control = TurnExecutionControl::new();
    let mut observed = source(path.to_str().unwrap(),&crate::tool_history::sha256(b"original"),0,8);
    observed["scope"]["environment"] = json!(environment);
    control.observe_delivered_coverage(&observed,None,0);
    let reference = format!("source:{}",control.acceptance_reads.keys().next().unwrap());
    let origin = crate::plan_store::AcceptedPlanInput::from_texts("task",["Read consumer"].into_iter()).references()[0].clone();
    control.refresh_plan(Some(crate::plan_store::PlanExecutionSnapshot {
        revision:"plan".into(),obligations:Default::default(),has_steps:false,acceptance_reviewed_revision:Some("input".into()),
        acceptance:BTreeMap::from([("read".into(),AcceptanceObligation { text:"Read consumer".into(),origin,
            requirement:EvidenceRequirement::Inspection { environment_id:environment,path:path.to_string_lossy().into_owned(),start:0,end:8 },
            plan_requirement_ids:Vec::new(),evidence_ref:Some(reference.clone()),blocker:None,retired_by:None,retirement_reason:None })]),
    }));
    assert!(control.fenced_acceptance_evidence(&step,"task",0).await.observations.contains_key(&reference));
    std::fs::write(&path,"external change").unwrap();
    assert!(!control.fenced_acceptance_evidence(&step,"task",0).await.observations.contains_key(&reference));
}

#[tokio::test]
async fn validation_source_binds_at_the_validating_settlement_not_later_naming() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("consumer.rs");
    let (_,mut turn) = crate::session::tests::make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
    let environment = step.environments.primary().unwrap().environment_id.clone();
    let origin = crate::plan_store::AcceptedPlanInput::from_texts("task",["Verify consumer"].into_iter()).references()[0].clone();
    let snapshot = |evidence_ref: Option<String>| Some(crate::plan_store::PlanExecutionSnapshot {
        revision:"plan".into(),obligations:Default::default(),has_steps:false,acceptance_reviewed_revision:Some("input".into()),
        acceptance:BTreeMap::from([("consumer".into(),AcceptanceObligation { text:"Verify consumer".into(),origin:origin.clone(),
            requirement:EvidenceRequirement::Validation { environment_id:environment.clone(),path:path.to_string_lossy().into_owned(),
                test_execution:false,required_tests:Default::default() },
            plan_requirement_ids:Vec::new(),evidence_ref,blocker:None,retired_by:None,retirement_reason:None })]),
    });
    for external_edit in [true,false] {
        std::fs::write(&path,"validated").unwrap();
        let mut control = TurnExecutionControl::new();
        control.validation_coverage_revision = Some(0);
        control.validation_coverage.insert("consumer".into(),ValidationScope {
            environment_id:environment.clone(),paths:BTreeSet::from([SourceDependencyV1::new(&path,false)]),
            test_execution:false,passed_tests:BTreeSet::new(),behavioral_paths:BTreeSet::new(),dependency_scope:None });
        // Validating settlement: the passing check exists before the model can name it.
        control.refresh_plan(snapshot(None));
        control.capture_acceptance_sources(&step,"task",0).await;
        if external_edit {
            std::fs::write(&path,"unowned edit after validation").unwrap();
        }
        // The next settlement follows the update_plan call that names the check.
        control.refresh_plan(snapshot(Some("check:consumer".into())));
        control.capture_acceptance_sources(&step,"task",0).await;
        let fenced = control.fenced_acceptance_evidence(&step,"task",0).await;
        assert_eq!(fenced.observations.contains_key("check:consumer"),!external_edit,
            "the binding must hold the validated bytes, not those present when the pointer was named");
    }
}
