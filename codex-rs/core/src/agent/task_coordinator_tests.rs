use super::*;
use crate::agent::task_metrics::MAX_RECORDED_EVENTS;
use codex_agent_task_store::AcceptanceCriterion;
use codex_agent_task_store::AgentRole;
use codex_agent_task_store::CapabilityProfile;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_protocol::ThreadId;
use tempfile::TempDir;

fn assignment_draft() -> AssignmentDraft {
    AssignmentDraft {
        root_session_id: "root-session".to_string(),
        admission_origin: codex_agent_task_store::AssignmentAdmissionOrigin::Typed,
        role: AgentRole::Worker,
        capability_profile: CapabilityProfile::ScopedSourceWrite,
        objective: "complete the task".to_string(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "criterion".to_string(),
            text: "criterion passes".to_string(),
        }],
        read_scope: Vec::new(),
        write_scope: Vec::new(),
        stop_condition: "task complete".to_string(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: vec!["cargo test -p codex-core".to_string()],
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: codex_agent_task_store::WorkspaceStrategy::Auto,
        relation: None,
        architecture_contract_ref: None,
    }
}

fn test_session_telemetry() -> SessionTelemetry {
    SessionTelemetry::new(
        ThreadId::new(),
        "test-model",
        "test-model",
        None,
        None,
        None,
        "test".to_string(),
        /*log_user_prompts*/ false,
        "unknown".to_string(),
        SessionSource::Cli,
    )
}

#[test]
fn bounded_diagnostics_deduplicate_progress_and_root_evidence_hydration() {
    let coordinator = AgentTaskCoordinator::default();
    let telemetry = test_session_telemetry();
    let attempt_id = AttemptId::new();
    let task_started_at = Utc::now();
    let progress_created_at = task_started_at + chrono::Duration::milliseconds(250);

    assert!(!coordinator.record_first_meaningful_progress_once(
        attempt_id,
        ObservationKind::Starting,
        &task_started_at,
        &progress_created_at,
        &telemetry,
    ));
    assert!(coordinator.record_first_meaningful_progress_once(
        attempt_id,
        ObservationKind::ToolCall,
        &task_started_at,
        &progress_created_at,
        &telemetry,
    ));
    assert!(!coordinator.record_first_meaningful_progress_once(
        attempt_id,
        ObservationKind::Mutation,
        &task_started_at,
        &progress_created_at,
        &telemetry,
    ));
    assert!(coordinator.record_root_receipt_hydration_once(attempt_id, &telemetry));
    assert!(!coordinator.record_root_receipt_hydration_once(attempt_id, &telemetry));

    let mut metrics = coordinator
        .metrics
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    metrics.first_progress_attempts.clear();
    metrics
        .first_progress_attempts
        .extend(std::iter::repeat_with(AttemptId::new).take(MAX_DIAGNOSTIC_ATTEMPT_IDENTITIES));
    drop(metrics);
    assert!(!coordinator.record_first_meaningful_progress_once(
        AttemptId::new(),
        ObservationKind::Validating,
        &task_started_at,
        &progress_created_at,
        &telemetry,
    ));
}

#[test]
fn task_progress_duration_uses_source_timestamps_and_clamps_clock_skew() {
    let task_started_at = Utc::now();
    let progress_created_at = task_started_at + chrono::Duration::milliseconds(250);
    assert_eq!(
        task_progress_duration(&task_started_at, &progress_created_at),
        std::time::Duration::from_millis(250)
    );
    assert_eq!(
        task_progress_duration(&progress_created_at, &task_started_at),
        std::time::Duration::ZERO
    );
}

#[tokio::test]
async fn workspace_coordination_lazy_state_initialization_is_singleflight() {
    let codex_home = TempDir::new().expect("codex home tempdir");
    let coordinator = AgentTaskCoordinator::default();
    let first = coordinator.initialize_for_workspace_coordination(
        None,
        codex_home.path().to_path_buf(),
        "test-provider".to_string(),
        "lazy-root".to_string(),
    );
    let second = coordinator.initialize_for_workspace_coordination(
        None,
        codex_home.path().to_path_buf(),
        "test-provider".to_string(),
        "lazy-root".to_string(),
    );

    let (first_result, second_result) = tokio::join!(first, second);

    first_result.expect("first lazy initialization");
    second_result.expect("parallel lazy initialization shares the same runtime");
    assert!(coordinator.store().is_some());
    assert_eq!(coordinator.root_session_id().as_deref(), Some("lazy-root"));
}

#[tokio::test]
async fn terminal_emission_uses_the_reserved_event_at_the_recorder_boundary() {
    let codex_home = TempDir::new().expect("codex home tempdir");
    let repository = TempDir::new().expect("repository tempdir");
    let state_runtime =
        StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("state runtime initializes");
    let coordinator = AgentTaskCoordinator::default();
    coordinator
        .initialize(state_runtime, "root-session".to_string())
        .await
        .expect("task coordinator initializes");
    let (assignment, attempt) = coordinator
        .create_assignment(repository.path(), assignment_draft())
        .await
        .expect("assignment is created");

    {
        let mut metrics = coordinator
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runtime = metrics
            .runtimes
            .get_mut(&assignment.assignment_id)
            .expect("metric runtime exists");
        for _ in 0..MAX_RECORDED_EVENTS - 1 {
            runtime
                .record_usage(/*tokens*/ 0, /*calls*/ 0)
                .expect("nonterminal event fits within the reserved boundary");
        }
    }

    coordinator
        .required_store()
        .expect("task store exists")
        .submit_agent_receipt(
            attempt.attempt_id,
            ReceiptDraft {
                status: AgentStatusClaim::NeedsMain,
                summary: "agent stopped without completing".to_string(),
                criterion_results: vec![CriterionResult {
                    criterion_id: "criterion".to_string(),
                    status: CriterionStatus::NotRun,
                    evidence: None,
                }],
                declared_changes: Vec::new(),
                validation_call_ids: Vec::new(),
                blockers: vec!["completion requires the main agent".to_string()],
                risks: Vec::new(),
                next_action: None,
                architecture_contract: None,
            },
        )
        .await
        .expect("receipt seals the attempt");
    coordinator.mark_task_inactive(assignment.assignment_id);

    let telemetry = test_session_telemetry();
    coordinator
        .maybe_emit_terminal_metrics(assignment.assignment_id, &telemetry)
        .await;
    coordinator
        .maybe_emit_terminal_metrics(assignment.assignment_id, &telemetry)
        .await;

    let metrics = coordinator
        .metrics
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!metrics.runtimes.contains_key(&assignment.assignment_id));
}
