async fn paged_native_source_acquisition_round_trip() -> std::time::Duration {
    use codex_agent_task_store::{AcceptanceCriterion, AgentRole, AgentTaskBindingDraft,
        AssignmentAdmissionOrigin, AssignmentDraft, CapabilityProfile, CriterionEvidenceKind,
        WorkspaceStrategy};
    use codex_protocol::{AgentPath, ThreadId};
    use codex_protocol::protocol::{SessionSource, SubAgentSource};
    use sha2::Digest;

    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = workspace.path().join("large.rs");
    // Non-JSON source cannot fit through a value-only selector. The file is
    // supported by read_file, but exceeds every single script response cap.
    let contents = "x".repeat(2 * 1024 * 1024);
    std::fs::write(&path, &contents).unwrap();
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    let root = session.thread_id.to_string();
    let coordinator = session.services.agent_control.task_coordinator();
    let state = codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".into())
        .await.unwrap();
    coordinator.initialize(state, root.clone()).await.unwrap();
    let (assignment, attempt) = coordinator.create_assignment(workspace.path(), AssignmentDraft {
        root_session_id: root,
        admission_origin: AssignmentAdmissionOrigin::Typed,
        role: AgentRole::Worker,
        capability_profile: CapabilityProfile::ScopedSourceWrite,
        objective: "Acquire the complete large source snapshot".into(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "acquisition".into(), text: "Acquire every source byte".into(),
        }],
        read_scope: Vec::new(), write_scope: Vec::new(),
        stop_condition: "All source bytes acquired".into(),
        dependencies: Vec::new(), risk_hints: Vec::new(),
        required_evidence: vec!["inspect:large.rs".into()],
        prohibited_changes: Vec::new(), contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Shared,
        relation: None, architecture_contract_ref: None,
    }).await.unwrap();
    let agent_path = AgentPath::root().join("source_reader").unwrap();
    coordinator.bind_agent_task(AgentTaskBindingDraft {
        assignment_id: assignment.assignment_id, attempt_id: attempt.attempt_id,
        agent_path: agent_path.to_string(), task_name: "source_reader".into(),
        thread_id: Some(session.thread_id.to_string()),
    }).await.unwrap();
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(), depth: 1, agent_path: Some(agent_path),
        agent_nickname: None, agent_role: None,
    });
    let store = coordinator.store().unwrap();
    let session = Arc::new(session);
    let step_context = session.capture_step_context(Arc::new(turn)).await.unwrap();
    let call = |name: &str, id: String, arguments: serde_json::Value| ToolInvocation {
        session: session.clone(), step_context: step_context.clone(),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: id.clone(), tool_name: ToolName::plain(name),
        source: ToolCallSource::CodeMode {
            cell_id: "inspection-pages".into(), parent_call_id: None,
            runtime_tool_call_id: id, nested_deadline: None, cancellation_cause: None,
        },
        payload: ToolPayload::Function { arguments: arguments.to_string() },
    };
    let initial = call("read_file", "inspection-source".into(), json!({"path": path}));
    let payload = initial.payload.clone();
    let started = std::time::Instant::now();
    let initial = ReadFileHandler.handle(initial).await.unwrap().code_mode_result(&payload);
    assert_eq!(initial["file_complete"], false);
    assert!(initial.get("criterion_evidence").is_none());
    let artifact = initial["artifact_id"].as_str().unwrap();
    let hash = initial["source_sha256"].as_str().unwrap();
    assert_eq!(hash, hex::encode(sha2::Sha256::digest(contents.as_bytes())));
    let mut acquired = initial["results"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(acquired.len() as u64, initial["results"][0]["canonical_range"]["end"].as_u64().unwrap());
    assert!(store.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap()
        .validation_calls.is_empty(), "retention and a first page are not full acquisition");
    while acquired.len() < contents.len() {
        let start = acquired.len();
        let end = (start + 256 * 1024).min(contents.len());
        let recovery = call("read_tool_output", format!("inspection-page-{start}"), json!({
            "artifact_id": artifact, "max_bytes": 1024 * 1024,
            "selectors": [{"kind": "bytes", "start": start, "end": end}],
        }));
        let payload = recovery.payload.clone();
        let page = ReadToolOutputHandler.handle(recovery).await.unwrap().code_mode_result(&payload);
        assert_eq!(page["complete"], true);
        assert_eq!(page["canonical_sha256"], hash);
        let text = page["results"][0]["text"].as_str().unwrap();
        assert_eq!(text.len(), end - start);
        acquired.push_str(text);
        if acquired.len() < contents.len() {
            assert!(page.get("criterion_evidence").is_none());
            assert!(store.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap()
                .validation_calls.is_empty(), "partial recovery must not create running or successful proof");
        } else {
            // Durable proof IDs are attempt-scoped, unlike runtime tool IDs.
            // Verify the referenced receipt, not its opaque storage spelling.
            let reference = page["criterion_evidence"]["call_id"].as_str().unwrap();
            let recorded = store.get_validation_call(reference.to_string()).await.unwrap().unwrap();
            assert_eq!(recorded.attempt_id, attempt.attempt_id);
            assert_eq!(recorded.verified_evidence_kind(), Some(CriterionEvidenceKind::SourceInspection));
            let observed = recorded.evidence.validation_result.as_ref().unwrap();
            assert_eq!(observed["path"], "large.rs");
            assert_eq!(observed["source_sha256"], hash);
            assert_eq!(observed["bytes"], contents.len());
        }
    }
    assert_eq!(acquired, contents, "every byte came from the immutable native snapshot");
    let task = store.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap();
    assert!(task.validation_calls.iter().any(|call|
        call.attempt_id == attempt.attempt_id
            && call.command_summary == "inspect:large.rs"
            && call.verified_evidence_kind() == Some(CriterionEvidenceKind::SourceInspection)),
        "complete acquisition must satisfy the bound task's inspect obligation, including paged reads");
    started.elapsed()
}

#[tokio::test]
async fn complete_paged_native_read_satisfies_typed_source_acquisition() {
    paged_native_source_acquisition_round_trip().await;
}

#[tokio::test]
#[ignore = "bounded native read/recovery/proof wall-clock benchmark"]
async fn paged_native_source_acquisition_wall_clock() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .finish(),
    );
    let mut samples = Vec::new();
    for _ in 0..3 {
        samples.push(paged_native_source_acquisition_round_trip().await.as_micros());
    }
    samples.sort_unstable();
    tracing::info!(bytes = 2097152, samples_us = ?samples, median_us = %samples[1], "paged_native_source_acquisition");
}
