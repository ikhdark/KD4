use super::*;
use crate::agent::task_metrics::MAX_RECORDED_EVENTS;
use codex_agent_task_store::AcceptanceCriterion;
use codex_agent_task_store::AgentRole;
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::CapabilityProfile;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::TaskActor;
use codex_agent_task_store::ValidationCallStatus;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SubAgentSource;
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

fn typed_source(path: &str) -> SessionSource {
    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: Some(AgentPath::try_from(path).expect("valid agent path")),
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    })
}

async fn initialized_coordinator() -> (AgentTaskCoordinator, TempDir, TempDir) {
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
    (coordinator, codex_home, repository)
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
async fn focused_validation_token_stays_pinned_across_source_rebinding() {
    let (coordinator, _codex_home, repository) = initialized_coordinator().await;
    let (first, first_attempt) = coordinator
        .create_assignment(repository.path(), assignment_draft())
        .await
        .expect("first assignment is created");
    coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: first.assignment_id,
            attempt_id: first_attempt.attempt_id,
            agent_path: "/root/worker".to_string(),
            task_name: "worker".to_string(),
            thread_id: None,
        })
        .await
        .expect("first assignment binds");
    let token = coordinator
        .begin_focused_validation_for_source(
            &typed_source("/root/worker"),
            "validation-call".to_string(),
            "cargo test -p codex-core".to_string(),
        )
        .await
        .expect("focused validation begins")
        .expect("typed source is bound");
    assert_eq!(token.assignment_id, first.assignment_id);

    let (second, second_attempt) = coordinator
        .create_assignment(repository.path(), assignment_draft())
        .await
        .expect("second assignment is created");
    let second_binding = coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: second.assignment_id,
            attempt_id: second_attempt.attempt_id,
            agent_path: "/root/worker_rebound".to_string(),
            task_name: "worker_rebound".to_string(),
            thread_id: None,
        })
        .await
        .expect("second assignment binds");
    coordinator.remember_binding(AgentTaskBinding {
        agent_path: "/root/worker".to_string(),
        task_name: "worker".to_string(),
        ..second_binding
    });
    assert_eq!(
        coordinator
            .binding_for_source(&typed_source("/root/worker"))
            .expect("source is rebound in the live index")
            .assignment_id,
        second.assignment_id
    );

    coordinator
        .finish_focused_validation(token, ValidationCallStatus::Succeeded)
        .await
        .expect("terminal result uses the pinned first attempt");
    let first_task = coordinator
        .get_agent_task(first.assignment_id, Some(0))
        .await
        .expect("first task reloads");
    assert_eq!(first_task.validation_calls.len(), 1);
    assert_eq!(
        first_task.validation_calls[0].status,
        ValidationCallStatus::Succeeded
    );
    assert!(
        coordinator
            .get_agent_task(second.assignment_id, Some(0))
            .await
            .expect("second task reloads")
            .validation_calls
            .is_empty()
    );
}

#[tokio::test]
async fn focused_validation_finish_after_seal_and_rebind_never_retargets() {
    let (coordinator, _codex_home, repository) = initialized_coordinator().await;
    let (first, first_attempt) = coordinator
        .create_assignment(repository.path(), assignment_draft())
        .await
        .expect("first assignment is created");
    coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: first.assignment_id,
            attempt_id: first_attempt.attempt_id,
            agent_path: "/root/worker".to_string(),
            task_name: "worker".to_string(),
            thread_id: None,
        })
        .await
        .expect("first assignment binds");
    let token = coordinator
        .begin_focused_validation_for_source(
            &typed_source("/root/worker"),
            "validation-call".to_string(),
            "cargo test -p codex-core".to_string(),
        )
        .await
        .expect("focused validation begins")
        .expect("typed source is bound");
    coordinator
        .required_store()
        .expect("task store exists")
        .abandon_agent_task(
            TaskActor::Root,
            first.assignment_id,
            "agent stopped".to_string(),
        )
        .await
        .expect("first task seals");
    coordinator
        .remove_agent_task_binding(first.assignment_id)
        .await
        .expect("sealed binding removal succeeds");

    let (second, second_attempt) = coordinator
        .create_assignment(repository.path(), assignment_draft())
        .await
        .expect("second assignment is created");
    coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: second.assignment_id,
            attempt_id: second_attempt.attempt_id,
            agent_path: "/root/worker".to_string(),
            task_name: "worker".to_string(),
            thread_id: None,
        })
        .await
        .expect("source rebinds to second assignment");

    assert!(
        coordinator
            .finish_focused_validation(token, ValidationCallStatus::Succeeded)
            .await
            .is_err()
    );
    assert!(
        coordinator
            .get_agent_task(second.assignment_id, Some(0))
            .await
            .expect("second task reloads")
            .validation_calls
            .is_empty()
    );
}

#[tokio::test]
async fn focused_validation_heartbeat_renews_the_running_call() {
    let (coordinator, _codex_home, repository) = initialized_coordinator().await;
    let (assignment, attempt) = coordinator
        .create_assignment(repository.path(), assignment_draft())
        .await
        .expect("assignment is created");
    coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: "/root/worker".to_string(),
            task_name: "worker".to_string(),
            thread_id: None,
        })
        .await
        .expect("assignment binds");
    let token = coordinator
        .begin_focused_validation_for_source(
            &typed_source("/root/worker"),
            "validation-heartbeat".to_string(),
            "cargo test -p codex-core focused".to_string(),
        )
        .await
        .expect("focused validation begins")
        .expect("typed source is bound");
    let store = coordinator.required_store().expect("task store exists");
    let initial_lease = store
        .get_validation_call("validation-heartbeat".to_string())
        .await
        .expect("validation call reads")
        .expect("validation call exists")
        .evidence
        .lease_expires_at
        .expect("running validation has a lease");

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        coordinator
            .heartbeat_focused_validation(&token)
            .await
            .expect("focused validation heartbeat succeeds")
    );

    let renewed_lease = store
        .get_validation_call("validation-heartbeat".to_string())
        .await
        .expect("renewed validation call reads")
        .expect("renewed validation call exists")
        .evidence
        .lease_expires_at
        .expect("renewed validation has a lease");
    assert!(renewed_lease > initial_lease);
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
