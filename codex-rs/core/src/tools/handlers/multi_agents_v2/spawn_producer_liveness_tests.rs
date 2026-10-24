use super::*;

#[tokio::test]
async fn audit151_persisted_active_assignment_is_not_a_running_producer() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let control = &session.services.agent_control;
    let coordinator = control.task_coordinator();
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let state = codex_state::StateRuntime::init(home.path().into(), "test".into())
        .await
        .unwrap();
    coordinator
        .initialize(Arc::clone(&state), "root".into())
        .await
        .unwrap();
    let (assignment, attempt) = coordinator
        .create_assignment(
            repo.path(),
            AssignmentDraft {
                root_session_id: "root".into(),
                admission_origin: AssignmentAdmissionOrigin::Typed,
                role: AgentRole::Explorer,
                capability_profile: AgentRole::Explorer.capability_profile(),
                objective: "inspect durable ownership".into(),
                acceptance_criteria: vec![AcceptanceCriterion {
                    evidence_policy: None,
                    id: "inspect".into(),
                    text: "report evidence".into(),
                }],
                read_scope: Vec::new(),
                write_scope: Vec::new(),
                stop_condition: "report findings".into(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: Vec::new(),
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: WorkspaceStrategy::Shared,
                relation: None,
                architecture_contract_ref: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(attempt.state, AttemptState::Active));
    for bound in [false, true] {
        if bound {
            coordinator
                .bind_agent_task(AgentTaskBindingDraft {
                    assignment_id: assignment.assignment_id,
                    attempt_id: attempt.attempt_id,
                    agent_path: "/root/explorer".into(),
                    task_name: "explorer".into(),
                    thread_id: Some(ThreadId::new().to_string()),
                })
                .await
                .unwrap();
        }
        let result = reusable_spawn_result(control, assignment.assignment_id, None).await;
        let Err(FunctionCallError::RespondToModel(message)) = result else {
            panic!("missing runtime must not return a reusable investigation");
        };
        assert!(
            message.contains("no verified running producer"),
            "{message}"
        );
        let retained = coordinator
            .get_agent_task(assignment.assignment_id, None)
            .await
            .unwrap();
        assert_eq!(retained.current_attempt.attempt_id, attempt.attempt_id);
        assert_eq!(retained.current_attempt.state, AttemptState::Active);
    }
    drop(turn);
    coordinator.store().unwrap().close().await;
    state.close().await;
}
