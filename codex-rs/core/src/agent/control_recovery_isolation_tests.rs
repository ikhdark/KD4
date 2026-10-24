use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn audit142_missing_cold_child_retains_obligation_without_blocking_healthy_sibling() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root, _) = harness.start_thread().await;
    harness.control.register_session_root(root, None);
    let state = harness.control.upgrade().unwrap();
    let graph = state.agent_graph_store().unwrap();
    let missing = ThreadId::new();
    graph.upsert_thread_spawn_edge(root, missing, ThreadSpawnEdgeStatus::Open).await.unwrap();
    let child = harness.control.spawn_agent_with_metadata(harness.config.clone(), text_input("healthy"), Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root, depth: 1, agent_path: Some(AgentPath::root().join("healthy").unwrap()), agent_nickname: None, agent_role: None,
    })), SpawnAgentOptions { parent_thread_id: Some(root), ..Default::default() }).await.unwrap();
    harness.control.restore_cold_descendants(root).await.unwrap();
    assert!(harness.control.task_coordinator().unavailable_descendants().iter().any(|(id, _)| id == &missing.to_string()));
    assert!(harness.control.get_agent_metadata(child.thread_id).is_some());
    assert!(graph.list_thread_spawn_children(root, Some(ThreadSpawnEdgeStatus::Open)).await.unwrap().contains(&missing));
    harness.control.shutdown_live_agent(child.thread_id).await.unwrap();
    harness.control.shutdown_live_agent(root).await.unwrap();
}

#[test]
fn audit142_task_capsule_inherits_selected_roots_without_parent_history() {
    run_current_thread_test_with_stack("audit142_task_capsule_inherits_selected_roots_without_parent_history", || async {
        let harness = AgentControlHarness::new().await;
        let roots = vec![SelectedCapabilityRoot {
            id: "selected@1".into(),
            location: CapabilityRootLocation::Environment { environment_id: "build".into(), path: PathUri::parse("file:///plugins/selected").unwrap() },
        }];
        let mut init = ExtensionDataInit::new();
        init.insert(roots.clone());
        let parent = harness.manager.start_thread_with_options(StartThreadOptions {
            config: harness.config.clone(), allow_provider_model_fallback: false,
            initial_history: InitialHistory::New, history_mode: None, session_source: None,
            thread_source: None, dynamic_tools: Vec::new(), metrics_service_name: None,
            parent_trace: None, environments: Vec::new(), thread_extension_init: init,
            supports_openai_form_elicitation: false,
        }).await.unwrap();
        parent.thread.inject_user_message_without_turn("private parent history marker".into()).await;
        let child = harness.control.spawn_agent_with_metadata(harness.config.clone(), text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id, depth: 1, agent_path: None,
                agent_nickname: None, agent_role: None,
            })), SpawnAgentOptions { fork_mode: Some(SpawnAgentForkMode::TaskCapsule), fork_parent_spawn_call_id: Some("capsule-spawn".into()), ..Default::default() }).await.unwrap();
        let thread = harness.manager.get_thread(child.thread_id).await.unwrap();
        assert_eq!(thread.codex.session.services.selected_capability_roots, roots);
        assert!(!history_contains_text(thread.codex.session.clone_history().await.raw_items(), "private parent history marker"));
        harness.control.shutdown_live_agent(child.thread_id).await.unwrap();
        harness.control.shutdown_live_agent(parent.thread_id).await.unwrap();
    });
}

#[tokio::test]
async fn audit142_corrupt_coordination_does_not_disable_root_analysis_or_full_suite_calls() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Kd4Runtime).unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    let directory = config.sqlite_home.join("agent-task-coordination");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("agent_tasks.sqlite"),
        b"not a sqlite database",
    )
    .unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root, thread) = harness.start_thread().await;
    assert!(harness.control.task_coordinator().store().is_none());
    let budget = harness
        .control
        .task_coordinator()
        .validation_budget_for_turn(&(root.to_string(), "test-turn".into()));
    assert!(
        budget.reserve(true, "suite-call".into()).is_ok(),
        "missing durable grants must not impose a full-suite launch allowance"
    );
    assert!(thread.is_running());
    let child = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("work"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root,
                depth: 1,
                agent_path: Some(AgentPath::root().join("worker").unwrap()),
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(root),
                ..Default::default()
            },
        )
        .await;
    assert!(
        child.is_err(),
        "a degraded root cannot start an uncoordinated child"
    );
    harness.control.shutdown_live_agent(root).await.unwrap();
}

#[tokio::test]
async fn audit142_csv_runtime_deadline_covers_stalled_publication_and_drains_worker() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent, _) = harness.start_thread().await;
    let job_home = TempDir::new().unwrap();
    let db = codex_state::StateRuntime::init(job_home.path().to_path_buf(), "test-provider".into())
        .await
        .unwrap();
    db.create_agent_job(
        &codex_state::AgentJobCreateParams {
            id: "deadline-job".into(),
            name: "deadline".into(),
            instruction: "work".into(),
            auto_export: false,
            max_runtime_seconds: Some(1),
            output_schema_json: None,
            input_headers: vec!["input".into()],
            input_csv_path: "unused.csv".into(),
            output_csv_path: "unused-output.csv".into(),
        },
        &[codex_state::AgentJobItemCreateParams {
            item_id: "row".into(),
            row_index: 0,
            source_id: None,
            row_json: serde_json::json!({"input":"work"}),
        }],
    )
    .await
    .unwrap();
    db.mark_agent_job_running("deadline-job").await.unwrap();
    let barrier = Arc::new(AgentControlTestBarrier::default());
    *harness
        .control
        .test_hooks
        .after_thread_created
        .lock()
        .unwrap() = Some(barrier.clone());
    let mut published = harness.manager.subscribe_thread_created();
    let control = harness.control.clone();
    let config = harness.config.clone();
    let job_db = db.clone();
    let spawn = tokio::spawn(async move {
        control
            .spawn_agent_with_metadata(
                config,
                text_input("work"),
                Some(SessionSource::SubAgent(SubAgentSource::Other(
                    "agent_job:deadline-job".into(),
                ))),
                SpawnAgentOptions {
                    parent_thread_id: Some(parent),
                    agent_job_binding: Some(AgentJobBinding {
                        state_db: job_db,
                        job_id: "deadline-job".into(),
                        item_id: "row".into(),
                        runtime_timeout: Some(Duration::from_secs(1)),
                    }),
                    ..Default::default()
                },
            )
            .await
    });
    timeout(Duration::from_secs(5), barrier.wait_until_reached())
        .await
        .unwrap();
    let worker = published.recv().await.unwrap();
    let error = timeout(Duration::from_secs(5), spawn)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("max runtime"), "{error}");
    timeout(Duration::from_secs(10), async {
        while harness.manager.get_thread(worker).await.is_ok() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout must keep cleanup ownership until the worker drains");
    *harness
        .control
        .test_hooks
        .after_thread_created
        .lock()
        .unwrap() = None;
    assert!(
        db.mark_agent_job_item_pending("deadline-job", "row", None)
            .await
            .unwrap()
    );
    let before = tokio::time::Instant::now();
    let healthy = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("work"),
            Some(SessionSource::SubAgent(SubAgentSource::Other(
                "agent_job:deadline-job".into(),
            ))),
            SpawnAgentOptions {
                parent_thread_id: Some(parent),
                agent_job_binding: Some(AgentJobBinding {
                    state_db: db.clone(),
                    job_id: "deadline-job".into(),
                    item_id: "row".into(),
                    runtime_timeout: Some(Duration::from_secs(5)),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(healthy.execution_started_at >= before);
    assert!(healthy.execution_started_at <= tokio::time::Instant::now());
    harness
        .control
        .shutdown_live_agent(healthy.thread_id)
        .await
        .unwrap();
    harness.control.shutdown_live_agent(parent).await.unwrap();
    db.close().await;
}
