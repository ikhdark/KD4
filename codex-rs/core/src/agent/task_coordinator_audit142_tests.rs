use super::*;

#[tokio::test]
async fn audit142_damaged_terminal_sibling_cannot_abort_healthy_reconciliation() {
    use codex_protocol::protocol::{EventMsg, TurnCompleteEvent};
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let state = StateRuntime::init(home.path().to_path_buf(), "test-provider".into())
        .await
        .unwrap();
    let owner = AgentTaskCoordinator::default();
    owner
        .initialize(state.clone(), "root-session".into())
        .await
        .unwrap();
    let (assignment, attempt) = owner
        .create_assignment(repo.path(), assignment_draft())
        .await
        .unwrap();
    let thread = ThreadId::new();
    let binding = owner
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: "/root/healthy".into(),
            task_name: "healthy".into(),
            thread_id: Some(thread.to_string()),
        })
        .await
        .unwrap();
    let event = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "retained-turn".into(),
        last_agent_message: Some("retained result".into()),
        surfaced_result: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
        timing: None,
    });
    owner
        .retain_terminal_outcome(
            binding,
            "retained-turn".into(),
            &AgentStatus::Completed(Some("retained result".into())),
            &event,
        )
        .await
        .unwrap();
    let (damaged, damaged_attempt) = owner
        .create_assignment(repo.path(), assignment_draft())
        .await
        .unwrap();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(
                home.path()
                    .join("agent-task-coordination/agent_tasks.sqlite"),
            ),
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO pending_terminal_outcomes VALUES (?, ?, '{', 'damaged-turn', '{}', '{}')",
    )
    .bind(damaged_attempt.attempt_id.to_string())
    .bind(damaged.assignment_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let restored = AgentTaskCoordinator::default();
    restored
        .initialize(state.clone(), "root-session".into())
        .await
        .unwrap();
    let task = restored
        .get_agent_task(assignment.assignment_id, None)
        .await
        .unwrap();
    assert!(task.receipt.unwrap().summary.contains("retained result"));
    assert!(
        task.observations
            .iter()
            .any(|observation| observation.call_id.as_deref()
                == Some(format!("{thread}:retained-turn").as_str()))
    );
    let pending = restored
        .required_store()
        .unwrap()
        .pending_terminal_outcomes("root-session")
        .await
        .unwrap();
    assert_eq!(
        pending.len(),
        2,
        "neither a malformed row nor an undelivered event may be erased"
    );
    assert_eq!(pending.iter().filter(|record| record.is_err()).count(), 1);
    pool.close().await;
    owner.required_store().unwrap().close().await;
    restored.required_store().unwrap().close().await;
    state.close().await;
}

#[tokio::test]
async fn audit142_terminal_recovery_cancels_only_expired_operations() {
    use codex_agent_task_store::{ValidationCall, ValidationCallStatus, ValidationEvidence};
    for expired in [false, true] {
        let home = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let state = StateRuntime::init(home.path().to_path_buf(), "test-provider".into())
            .await
            .unwrap();
        let coordinator = AgentTaskCoordinator::default();
        coordinator
            .initialize(state.clone(), "root-session".into())
            .await
            .unwrap();
        let draft = assignment_draft();
        let command = draft.required_evidence[0].clone();
        let (assignment, attempt) = coordinator
            .create_assignment(repo.path(), draft)
            .await
            .unwrap();
        let path = AgentPath::root().join("worker").unwrap();
        let thread = ThreadId::new();
        coordinator
            .bind_agent_task(AgentTaskBindingDraft {
                assignment_id: assignment.assignment_id,
                attempt_id: attempt.attempt_id,
                agent_path: path.to_string(),
                task_name: "worker".into(),
                thread_id: Some(thread.to_string()),
            })
            .await
            .unwrap();
        let store = coordinator.required_store().unwrap();
        store
            .record_validation_call(ValidationCall {
                call_id: "owned-operation".into(),
                attempt_id: attempt.attempt_id,
                command_summary: command,
                status: ValidationCallStatus::Running,
                evidence: ValidationEvidence::default(),
                recorded_at: Utc::now(),
            })
            .await
            .unwrap();
        if expired {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new().filename(
                        home.path()
                            .join("agent-task-coordination/agent_tasks.sqlite"),
                    ),
                )
                .await
                .unwrap();
            sqlx::query("UPDATE validation_calls SET body_json = json_set(body_json, '$.evidence.lease_expires_at', ?) WHERE call_id = 'owned-operation'")
                .bind((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339())
                .execute(&pool).await.unwrap();
            pool.close().await;
        }
        let result = coordinator
            .seal_missing_receipt_for_attempt(
                &path,
                thread,
                &AgentStatus::Completed(Some("execution ended".into())),
                attempt.attempt_id,
            )
            .await;
        let task = coordinator
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap();
        if expired {
            assert_eq!(result.unwrap().unwrap().status, AgentStatusClaim::Abandoned);
            assert_eq!(
                task.validation_calls[0].status,
                ValidationCallStatus::Cancelled
            );
            assert!(
                task.receipt
                    .unwrap()
                    .criterion_results
                    .iter()
                    .all(|criterion| criterion.status == CriterionStatus::NotRun)
            );
        } else {
            assert!(matches!(
                result,
                Err(StoreError::ValidationCallStatusInvalid { .. })
            ));
            assert!(task.receipt.is_none());
            assert_eq!(
                task.validation_calls[0].status,
                ValidationCallStatus::Running
            );
        }
        store.close().await;
        state.close().await;
    }
}

async fn audit142_retain_completed_outcome(
    coordinator: &AgentTaskCoordinator,
    repo: &std::path::Path,
    name: &str,
    result: &str,
) -> AssignmentId {
    use codex_protocol::protocol::{EventMsg, TurnCompleteEvent};
    let (assignment, attempt) = coordinator
        .create_assignment(repo, assignment_draft())
        .await
        .unwrap();
    let binding = coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: format!("/root/{name}"),
            task_name: name.into(),
            thread_id: Some(ThreadId::new().to_string()),
        })
        .await
        .unwrap();
    let turn_id = format!("{name}-turn");
    let event = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.clone(),
        last_agent_message: Some(result.into()),
        surfaced_result: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
        timing: None,
    });
    coordinator
        .retain_terminal_outcome(
            binding,
            turn_id,
            &AgentStatus::Completed(Some(result.into())),
            &event,
        )
        .await
        .unwrap();
    assignment.assignment_id
}

#[tokio::test]
async fn audit142_recovered_terminal_outcome_is_not_rereconciled_each_pass() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let state = StateRuntime::init(home.path().to_path_buf(), "test-provider".into())
        .await
        .unwrap();
    let coordinator = AgentTaskCoordinator::default();
    coordinator
        .initialize(state.clone(), "root-session".into())
        .await
        .unwrap();
    let first =
        audit142_retain_completed_outcome(&coordinator, repo.path(), "first", "first result")
            .await;
    // The first pass after retention recovers the receipt and keeps the
    // undelivered event for its delivery owner.
    coordinator
        .initialize(state.clone(), "root-session".into())
        .await
        .unwrap();
    assert!(
        coordinator
            .get_agent_task(first, None)
            .await
            .unwrap()
            .receipt
            .unwrap()
            .summary
            .contains("first result")
    );
    let store = coordinator.required_store().unwrap();
    let settled = store
        .task_state_revision_for_root("root-session")
        .await
        .unwrap();
    // Work count: every later pass used to commit no-op attempt writes.
    coordinator
        .initialize(state.clone(), "root-session".into())
        .await
        .unwrap();
    coordinator.recover_unstarted_assignments().await.unwrap();
    assert_eq!(
        store
            .task_state_revision_for_root("root-session")
            .await
            .unwrap(),
        settled,
        "an already-recovered outcome is not rewritten on later passes"
    );
    assert_eq!(
        store
            .pending_terminal_outcomes("root-session")
            .await
            .unwrap()
            .len(),
        1,
        "the undelivered event stays retained for its delivery owner"
    );
    let second =
        audit142_retain_completed_outcome(&coordinator, repo.path(), "second", "second result")
            .await;
    coordinator.recover_unstarted_assignments().await.unwrap();
    assert!(
        coordinator
            .get_agent_task(second, None)
            .await
            .unwrap()
            .receipt
            .unwrap()
            .summary
            .contains("second result"),
        "a newly retained outcome is still recovered on the next pass"
    );
    store.close().await;
    state.close().await;
}
