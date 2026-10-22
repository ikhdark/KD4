use super::CompletedUnloadResult;
use super::UnloadOneResult;
use super::V2Residency;
use crate::ThreadManager;
use crate::agent::AgentControl;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::ThreadManagerState;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn eviction_claim_stays_charged_and_touch_waits_for_completion() {
    let residency = Arc::new(V2Residency::default());
    let thread_id = ThreadId::new();
    assert!(residency.try_reserve_pending_slot(/*capacity*/ 1));
    residency.commit_slot(thread_id);

    let claim = residency
        .claim_lru_candidate(/*protected_thread_id*/ None)
        .expect("claim resident for eviction");
    assert!(
        !residency.try_reserve_pending_slot(/*capacity*/ 1),
        "an eviction in progress must remain charged against capacity"
    );

    let mut waiter = Box::pin(residency.wait_for_eviction_or_touch(thread_id));
    assert!(
        futures::poll!(waiter.as_mut()).is_pending(),
        "touch must wait for eviction"
    );

    let pending_slot = claim.into_pending_slot();
    assert!(
        !waiter.await,
        "completed eviction must make the caller recheck and reload"
    );
    assert!(
        !residency.try_reserve_pending_slot(/*capacity*/ 1),
        "the eviction occupancy must transfer atomically to the requesting slot"
    );

    {
        let state = residency
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!state.residents.contains(&thread_id));
        assert!(!state.evicting.contains_key(&thread_id));
    }

    drop(pending_slot);
    assert!(residency.try_reserve_pending_slot(/*capacity*/ 1));
}

#[tokio::test]
async fn residency_slot_reservation_unloads_oldest_idle_v2_agent() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker_1").await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(&state, &config, None)
        .await
        .expect("second slot");
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker_2").await;
    second_slot.commit(second.thread_id);
    mark_thread_completed(second.thread.as_ref()).await;
    assert!(
        control
            .touch_loaded_v2_residency(&state, first.thread_id)
            .await
    );
    let replacement = control
        .reserve_v2_residency_slot(&state, &config, None)
        .await
        .expect("evict LRU");
    assert!(matches!(manager.get_thread(second.thread_id).await,
        Err(CodexErr::ThreadNotFound(id)) if id == second.thread_id));
    assert!(
        manager.get_thread(first.thread_id).await.is_ok(),
        "recently touched resident survives"
    );
    assert!(manager.get_thread(root.thread_id).await.is_ok());
    drop(replacement);
}

#[tokio::test]
async fn smaller_residency_limits_reject_existing_resident_replacement() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let home = tempfile::tempdir().unwrap();
    config.codex_home = home.path().to_path_buf().try_into().unwrap();
    config.cwd = home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager.start_thread(config.clone()).await.unwrap();
    let control = manager.agent_control();
    let state = control.upgrade().unwrap();
    let mut residents = Vec::new();
    for name in ["a", "b"] {
        let slot = control
            .reserve_v2_residency_slot(&state, &config, None)
            .await
            .unwrap();
        let child = spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, name).await;
        slot.commit(child.thread_id);
        mark_thread_completed(child.thread.as_ref()).await;
        residents.push(child.thread_id);
    }
    let mut smaller = config.clone();
    smaller.multi_agent_v2.max_concurrent_threads_per_session = 1;
    assert!(matches!(
        control
            .reserve_v2_residency_slot(&state, &smaller, None)
            .await,
        Err(CodexErr::AgentLimitReached { max_threads: 0 })
    ));
    for id in &residents {
        assert!(
            manager.get_thread(*id).await.is_ok(),
            "zero capacity must not cause an eviction"
        );
    }
    smaller.multi_agent_v2.max_concurrent_threads_per_session = 2;
    assert!(
        matches!(
            control
                .reserve_v2_residency_slot(&state, &smaller, None)
                .await,
            Err(CodexErr::AgentLimitReached { max_threads: 1 })
        ),
        "one-for-one replacement cannot bypass a smaller supplied cap"
    );
}

#[tokio::test]
async fn residency_materialization_failure_preserves_running_agent_and_buffered_history() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");
    let slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = spawn_v2_subagent(
        &control,
        &state,
        config.clone(),
        root.thread_id,
        "persistence_worker",
    )
    .await;
    slot.commit(first.thread_id);
    let rollout_path = first
        .thread
        .codex
        .session
        .current_rollout_path()
        .await
        .expect("resolve rollout path")
        .expect("local rollout path");
    assert!(!rollout_path.exists(), "new rollout must still be deferred");
    first
        .thread
        .codex
        .session
        .live_thread()
        .expect("live thread store")
        .append_items_ordered(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
            AgentMessageEvent {
                message: "history-before-failed-eviction".to_string(),
                phase: None,
            },
        ))])
        .await
        .expect("queue history before eviction");
    mark_thread_completed(first.thread.as_ref()).await;

    // A directory at the deferred file path makes the real store's persist fail.
    std::fs::create_dir_all(&rollout_path).expect("block rollout file creation");
    match control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
    {
        Err(CodexErr::AgentLimitReached { max_threads }) => assert_eq!(max_threads, 1),
        Err(err) => panic!("expected retained residency capacity, got {err:?}"),
        Ok(_) => panic!("failed persistence must not free the resident slot"),
    }
    let retained = manager
        .get_thread(first.thread_id)
        .await
        .expect("failed persistence must retain the runtime");
    assert!(Arc::ptr_eq(&retained, &first.thread));
    assert!(first.thread.is_running(), "shutdown must not be submitted");
    let mut terminated = Box::pin(first.thread.wait_until_terminated());
    assert!(futures::poll!(terminated.as_mut()).is_pending());
    drop(terminated);
    {
        let residency = control
            .v2_residency
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(residency.residents.contains(&first.thread_id));
        assert!(!residency.evicting.contains_key(&first.thread_id));
        assert_eq!(residency.pending_slots, 0);
    }

    std::fs::remove_dir(&rollout_path).expect("remove storage fault");
    let recovered_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("successful persistence must allow eviction after recovery");
    match manager.get_thread(first.thread_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first.thread_id),
        Err(err) => panic!("expected recovered eviction, got {err:?}"),
        Ok(_) => panic!("successfully persisted resident must be evicted"),
    }
    assert!(!first.thread.is_running());
    let (history, _, parse_errors) =
        codex_rollout::RolloutRecorder::load_rollout_items(&rollout_path)
            .await
            .expect("read persisted history");
    assert_eq!(parse_errors, 0, "invalid rollout records");
    let buffered_messages = history
        .iter()
        .filter(|item| {
            matches!(item, RolloutItem::EventMsg(EventMsg::AgentMessage(message))
                if message.message == "history-before-failed-eviction")
        })
        .count();
    assert_eq!(
        buffered_messages, 1,
        "failed eviction must preserve queued history exactly once"
    );
    drop(recovered_slot);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_residency_shutdown_keeps_claim_charged_until_slot_handoff() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let manager_state = control.upgrade().expect("thread manager should be live");
    let residency = Arc::clone(&control.v2_residency);

    let first_slot = control
        .reserve_v2_residency_slot(&manager_state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = spawn_v2_subagent(
        &control,
        &manager_state,
        config,
        root.thread_id,
        "late_worker",
    )
    .await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;
    let release_shutdown = crate::test_support::block_thread_terminal_tasks(first.thread.as_ref());

    let completion = match residency
        .try_unload_one_resident_with_shutdown_timeout(
            &manager_state,
            /*protected_thread_id*/ None,
            Duration::ZERO,
            &mut std::collections::HashSet::new(),
        )
        .await
    {
        UnloadOneResult::WaitingForLateShutdown(completion) => completion,
        _ => panic!("short deadline should hand ownership to late shutdown cleanup"),
    };

    {
        let state = residency
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!state.residents.contains(&first.thread_id));
        assert!(state.evicting.contains_key(&first.thread_id));
        assert_eq!(state.pending_slots, 0);
    }
    assert!(
        !residency.try_reserve_pending_slot(/*capacity*/ 1),
        "the late eviction claim must stay charged"
    );
    let mut touch = Box::pin(residency.wait_for_eviction_or_touch(first.thread_id));
    assert!(
        futures::poll!(touch.as_mut()).is_pending(),
        "touch must remain blocked while late cleanup owns the claim"
    );
    assert!(
        manager.get_thread(first.thread_id).await.is_ok(),
        "the manager entry must remain until termination"
    );

    release_shutdown
        .send(())
        .expect("blocked terminal task should still be waiting");
    let reserved_slot = match tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .expect("late cleanup should finish after terminal work")
        .expect("late cleanup owner should return its slot")
    {
        CompletedUnloadResult::Reserved(slot) => slot,
        CompletedUnloadResult::Retry => panic!("late cleanup should remove the expected thread"),
    };
    assert!(!touch.await, "completed eviction must make touch reload");
    match manager.get_thread(first.thread_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first.thread_id),
        Err(err) => panic!("expected evicted thread to be missing, got {err:?}"),
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    {
        let state = residency
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!state.evicting.contains_key(&first.thread_id));
        assert_eq!(state.pending_slots, 1);
    }

    drop(reserved_slot);
    assert!(residency.try_reserve_pending_slot(/*capacity*/ 1));
}

#[tokio::test]
async fn registered_interrupted_v2_agent_reloads_after_residency_eviction() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let child_dir = temp_home.path().join("isolated-child");
    std::fs::create_dir_all(&child_dir).unwrap();
    let mut child_config = config.clone();
    child_config.cwd = child_dir.clone().try_into().unwrap();
    child_config.developer_instructions = Some("child-specific instructions".into());
    child_config.model_reasoning_effort = Some(codex_protocol::openai_models::ReasoningEffort::Low);
    let first = spawn_v2_subagent(&control, &state, child_config, root.thread_id, "worker_1").await;
    first_slot.commit(first.thread_id);
    // This synthetic interruption bypasses normal turn preparation, which
    // records the complete sticky settings before publishing turn events.
    first
        .thread
        .codex
        .session
        .persist_thread_settings_snapshot()
        .await
        .unwrap();
    mark_thread_interrupted(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("second resident slot should evict the first interrupted idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first.thread_id),
        Err(err) => panic!("expected evicted thread to be missing, got {err:?}"),
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let cold_agents = control
        .list_agents(&SessionSource::Cli, Some("/root/worker_1"))
        .await
        .unwrap();
    assert_eq!(
        cold_agents.len(),
        1,
        "cold registry identity remains visible without a receipt"
    );
    assert!(!cold_agents[0].runtime_loaded);
    assert_eq!(cold_agents[0].agent_name, "/root/worker_1");
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker_2").await;
    second_slot.commit(second.thread_id);
    mark_thread_completed(second.thread.as_ref()).await;

    assert!(
        control
            .state
            .agent_metadata_for_thread(first.thread_id)
            .is_some(),
        "eviction preserves reload identity"
    );
    // The evicted runtime's storage lease lives as long as its last strong handle.
    drop(first.thread);
    control
        .ensure_v2_agent_loaded(config.clone(), first.thread_id)
        .await
        .expect("registered interrupted history can reload");
    assert!(manager.get_thread(first.thread_id).await.is_ok());
    // A completed child follows the same reload boundary, evicting the interrupted child.
    let reloaded = manager.get_thread(first.thread_id).await.unwrap();
    let settings = reloaded.config_snapshot().await;
    // The default test manager selects its local executor at the requested child
    // cwd. The full settings receipt must preserve that selection and fallback.
    assert_eq!(
        settings.environments.environments,
        vec![codex_protocol::protocol::TurnEnvironmentSelection {
            environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            cwd: codex_utils_path_uri::PathUri::from_host_native_path(&child_dir).unwrap(),
        },]
    );
    assert_eq!(
        settings.environments.legacy_fallback_cwd.as_path(),
        child_dir.as_path()
    );
    assert_eq!(
        settings.developer_instructions.as_deref(),
        Some("child-specific instructions")
    );
    assert_eq!(
        settings.reasoning_effort,
        Some(codex_protocol::openai_models::ReasoningEffort::Low)
    );
    mark_thread_completed(reloaded.as_ref()).await;
    drop(reloaded);
    drop(second.thread);
    control
        .ensure_v2_agent_loaded(config, second.thread_id)
        .await
        .expect("completed history reloads");
    assert!(manager.get_thread(second.thread_id).await.is_ok());
    assert!(matches!(manager.get_thread(first.thread_id).await,
        Err(CodexErr::ThreadNotFound(id)) if id == first.thread_id));
    assert!(manager.get_thread(root.thread_id).await.is_ok());
}

#[tokio::test]
async fn duplicate_v2_agent_path_is_rejected_before_residency_eviction() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");
    let agent_path = AgentPath::try_from("/root/worker").expect("valid agent path");

    let mut spawn_reservation = control
        .state
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve spawn slot");
    let (session_source, mut agent_metadata) = control
        .prepare_thread_spawn(
            &mut spawn_reservation,
            &config,
            root.thread_id,
            /*depth*/ 1,
            Some(agent_path.clone()),
            /*agent_role*/ None,
            /*preferred_agent_nickname*/ None,
        )
        .expect("prepare resident spawn");
    let residency_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("reserve resident slot");
    let resident = state
        .spawn_new_thread_with_source(
            config.clone(),
            control.clone(),
            session_source,
            Some(root.thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("spawn resident agent");
    agent_metadata.agent_id = Some(resident.thread_id);
    spawn_reservation
        .commit(agent_metadata)
        .expect("commit resident spawn reservation");
    residency_slot.commit(resident.thread_id);
    mark_thread_completed(resident.thread.as_ref()).await;

    let duplicate_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(agent_path),
        agent_nickname: None,
        agent_role: None,
    });
    let err = control
        .spawn_agent(config.clone(), vec![], Some(duplicate_source))
        .await
        .expect_err("duplicate agent path should be rejected");
    match err {
        CodexErr::UnsupportedOperation(message) => {
            assert!(
                message.contains("already exists"),
                "unexpected error: {message}"
            );
        }
        err => panic!("expected duplicate-path error, got {err:?}"),
    }

    assert!(
        manager.get_thread(resident.thread_id).await.is_ok(),
        "duplicate-path rejection must not evict the existing resident"
    );
    let new_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(AgentPath::root().join("invalid").unwrap()),
        agent_nickname: None,
        agent_role: None,
    });
    let cases = [
        (
            Some(new_source.clone()),
            crate::agent::control::SpawnAgentOptions {
                fork_mode: Some(crate::agent::control::SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
            "parent spawn call id",
        ),
        (
            Some(SessionSource::SubAgent(SubAgentSource::Other(
                "invalid".to_string(),
            ))),
            crate::agent::control::SpawnAgentOptions {
                fork_mode: Some(crate::agent::control::SpawnAgentForkMode::FullHistory),
                fork_parent_spawn_call_id: Some("call".to_string()),
                ..Default::default()
            },
            "thread-spawn session source",
        ),
        (
            Some(new_source),
            crate::agent::control::SpawnAgentOptions {
                typed_task_binding: Some(codex_agent_task_store::AgentTaskBindingDraft {
                    assignment_id: codex_agent_task_store::AssignmentId::new(),
                    attempt_id: codex_agent_task_store::AttemptId::new(),
                    agent_path: "/root/different".to_string(),
                    task_name: "invalid".to_string(),
                    thread_id: None,
                }),
                ..Default::default()
            },
            "does not match spawned agent path",
        ),
    ];
    for (source, options, message) in cases {
        let error = control
            .spawn_agent_with_metadata(config.clone(), Vec::new(), source, options)
            .await
            .expect_err("invalid request must fail before eviction");
        assert!(
            error.to_string().contains(message),
            "unexpected rejection: {error}"
        );
        assert!(
            manager.get_thread(resident.thread_id).await.is_ok(),
            "rejected request evicted resident"
        );
    }
}

async fn spawn_v2_subagent(
    control: &AgentControl,
    state: &Arc<ThreadManagerState>,
    config: Config,
    parent_thread_id: ThreadId,
    label: &str,
) -> crate::thread_manager::NewThread {
    let parent_turn = state
        .get_thread(parent_thread_id)
        .await
        .expect("parent")
        .codex
        .session
        .new_default_turn()
        .await;
    let mut reservation = control
        .state
        .reserve_spawn_slot(None)
        .expect("registry reservation");
    let (source, mut metadata) = control
        .prepare_thread_spawn(
            &mut reservation,
            &config,
            parent_thread_id,
            crate::agent::next_thread_spawn_depth(&parent_turn.session_source),
            Some(
                control
                    .state
                    .agent_metadata_for_thread(parent_thread_id)
                    .and_then(|metadata| metadata.agent_path)
                    .unwrap_or_else(AgentPath::root)
                    .join(label)
                    .expect("child path"),
            ),
            None,
            None,
        )
        .expect("prepare registered child");
    let child = state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            source,
            Some(parent_thread_id),
            None,
            Some(ThreadSource::Subagent),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("spawn v2 subagent");
    metadata.agent_id = Some(child.thread_id);
    reservation
        .commit(metadata)
        .expect("register child identity");
    child
}

async fn mark_thread_completed(thread: &CodexThread) {
    let turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                surfaced_result: None,
                turn_id: turn.sub_id.clone(),
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                timing: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn mark_thread_interrupted(thread: &CodexThread) {
    let turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn.sub_id.clone()),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
                timing: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn clear_active_turn(thread: &CodexThread) {
    // The fixture has no task runner to clear the turn after the terminal event.
    *thread.codex.session.active_turn.lock().await = None;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn residency_cancellation_retains_cleanup_until_termination() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let manager_state = control.upgrade().expect("thread manager should be live");
    let residency = Arc::clone(&control.v2_residency);

    let first_slot = control
        .reserve_v2_residency_slot(&manager_state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = spawn_v2_subagent(
        &control,
        &manager_state,
        config.clone(),
        root.thread_id,
        "late_worker",
    )
    .await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;
    let release_shutdown = crate::test_support::block_thread_terminal_tasks(first.thread.as_ref());

    let owner = Arc::clone(&residency);
    let state = Arc::clone(&manager_state);
    let reservation = tokio::spawn(async move { owner.reserve_slot(&state, 1, None).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !first.thread.codex.session.terminal_tasks.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown accepted before cancelling reservation");
    reservation.abort();
    assert!(matches!(reservation.await, Err(error) if error.is_cancelled()));
    assert!(!residency.try_reserve_pending_slot(1));
    {
        let state = residency.state.lock().unwrap();
        assert!(state.evicting.contains_key(&first.thread_id));
        assert!(!state.residents.contains(&first.thread_id));
    }
    let mut touch = Box::pin(residency.wait_for_eviction_or_touch(first.thread_id));
    assert!(futures::poll!(touch.as_mut()).is_pending());
    release_shutdown.send(()).expect("release terminal task");
    assert!(
        !tokio::time::timeout(Duration::from_secs(5), touch)
            .await
            .expect("owned cleanup finishes")
    );
    assert!(matches!(manager.get_thread(first.thread_id).await,
        Err(CodexErr::ThreadNotFound(id)) if id == first.thread_id));
    // Channel disposal also drops the unclaimed pending slot.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !residency.try_reserve_pending_slot(1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("occupancy released without another eviction");
    residency.release_pending_slot();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn residency_foreground_timeout_retains_cleanup_until_termination() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let manager_state = control.upgrade().expect("thread manager should be live");
    let residency = Arc::clone(&control.v2_residency);

    let first_slot = control
        .reserve_v2_residency_slot(&manager_state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = spawn_v2_subagent(
        &control,
        &manager_state,
        config.clone(),
        root.thread_id,
        "late_worker",
    )
    .await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;
    let release_shutdown = crate::test_support::block_thread_terminal_tasks(first.thread.as_ref());

    assert!(matches!(
        Arc::clone(&residency)
            .reserve_slot_with_shutdown_timeout(&manager_state, 1, None, Duration::ZERO)
            .await,
        Err(CodexErr::AgentLimitReached { max_threads: 1 })
    ));
    assert!(!residency.try_reserve_pending_slot(1));
    {
        let state = residency.state.lock().unwrap();
        assert!(state.evicting.contains_key(&first.thread_id));
        assert!(!state.residents.contains(&first.thread_id));
    }
    let mut touch = Box::pin(residency.wait_for_eviction_or_touch(first.thread_id));
    assert!(futures::poll!(touch.as_mut()).is_pending());
    release_shutdown.send(()).expect("release terminal task");
    assert!(
        !tokio::time::timeout(Duration::from_secs(5), touch)
            .await
            .expect("owned cleanup finishes")
    );
    assert!(matches!(manager.get_thread(first.thread_id).await,
        Err(CodexErr::ThreadNotFound(id)) if id == first.thread_id));
    // Channel disposal also drops the unclaimed pending slot.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !residency.try_reserve_pending_slot(1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("occupancy released without another eviction");
    residency.release_pending_slot();
}

#[tokio::test]
async fn explicit_v2_resume_preserves_cold_identity_and_accounts_for_residency() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    config.sqlite_home = temp_home.path().to_path_buf();
    let state_db = crate::init_state_db(&config).await.expect("state database");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_db),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = root.thread.codex.session.services.agent_control.clone();
    let manager_state = control.upgrade().expect("thread manager should be live");
    let residency = Arc::clone(&control.v2_residency);

    let first_slot = control
        .reserve_v2_residency_slot(&manager_state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = spawn_v2_subagent(
        &control,
        &manager_state,
        config.clone(),
        root.thread_id,
        "late_worker",
    )
    .await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(&manager_state, &config, None)
        .await
        .expect("evict first");
    let second = spawn_v2_subagent(
        &control,
        &manager_state,
        config.clone(),
        root.thread_id,
        "second",
    )
    .await;
    second_slot.commit(second.thread_id);
    mark_thread_completed(second.thread.as_ref()).await;
    assert!(
        control
            .state
            .agent_metadata_for_thread(first.thread_id)
            .is_some()
    );
    assert_eq!(
        control
            .resume_agent_from_rollout(config.clone(), first.thread_id, SessionSource::Exec)
            .await
            .expect("explicit cold resume"),
        first.thread_id
    );
    assert!(
        control
            .state
            .agent_metadata_for_thread(second.thread_id)
            .is_some(),
        "unrelated cold identity survives"
    );
    assert!(matches!(
        manager.get_thread(second.thread_id).await,
        Err(CodexErr::ThreadNotFound(_))
    ));
    assert!(
        residency
            .state
            .lock()
            .unwrap()
            .residents
            .contains(&first.thread_id)
    );
    control
        .shutdown_live_agent(first.thread_id)
        .await
        .expect("close first runtime and registration");
    assert!(
        control
            .state
            .agent_metadata_for_thread(first.thread_id)
            .is_none()
    );
    let graph = manager_state
        .agent_graph_store()
        .expect("agent graph store");
    graph
        .set_thread_spawn_edge_status(
            first.thread_id,
            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("clear prior open edge before testing resume publication");
    let barrier = Arc::new(crate::agent::control::AgentControlTestBarrier::default());
    *control.test_hooks.after_thread_created.lock().unwrap() = Some(Arc::clone(&barrier));
    let resume_control = control.clone();
    let resume_config = config.clone();
    let thread_id = first.thread_id;
    let resume = tokio::spawn(async move {
        resume_control
            .resume_agent_from_rollout(resume_config, thread_id, SessionSource::Exec)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), barrier.wait_until_reached())
        .await
        .expect("direct resume reaches publication boundary");
    resume.abort();
    assert!(matches!(resume.await, Err(error) if error.is_cancelled()));
    // As in spawn, the durable edge commits before the resumed runtime is announced,
    // so an edge-write failure rolls back through pending cleanup.
    assert!(
        graph
            .list_thread_spawn_children(
                root.thread_id,
                Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
            )
            .await
            .expect("read open edges at the publication boundary")
            .contains(&first.thread_id)
    );
    barrier.release_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if graph
                .list_thread_spawn_children(
                    root.thread_id,
                    Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                )
                .await
                .expect("read published resume edge")
                .contains(&first.thread_id)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled resume still finishes durable publication");
    assert!(manager.get_thread(first.thread_id).await.is_ok());
    assert!(
        residency
            .state
            .lock()
            .unwrap()
            .residents
            .contains(&first.thread_id)
    );
    assert!(
        control
            .state
            .agent_metadata_for_thread(first.thread_id)
            .is_some()
    );
    assert!(
        control
            .state
            .agent_metadata_for_thread(second.thread_id)
            .is_some()
    );
    assert!(!residency.try_reserve_pending_slot(1));
    *control.test_hooks.after_thread_created.lock().unwrap() = None;
    control
        .shutdown_live_agent(root.thread_id)
        .await
        .expect("stop root while retaining children");
    assert_eq!(
        control
            .resume_agent_from_rollout(config, root.thread_id, SessionSource::Exec)
            .await
            .expect("root resume does not consume child residency capacity"),
        root.thread_id
    );
    let residents = residency.state.lock().unwrap();
    assert_eq!(
        residents.residents.iter().copied().collect::<Vec<_>>(),
        vec![first.thread_id]
    );
    assert_eq!(residents.pending_slots, 0);
}

#[tokio::test]
async fn touching_an_eviction_has_a_foreground_deadline_without_releasing_occupancy() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager.start_thread(config.clone()).await.unwrap();
    let control = manager.agent_control();
    let state = control.upgrade().unwrap();
    let child = spawn_v2_subagent(&control, &state, config, root.thread_id, "evicting").await;
    let residency = &control.v2_residency;
    assert!(residency.try_reserve_pending_slot(1));
    residency.commit_slot(child.thread_id);
    let claim = residency.claim_lru_candidate(None).unwrap();
    tokio::time::pause();
    let result = control
        .use_loaded_v2_agent_or_clear_stopped(&state, child.thread_id)
        .await;
    assert!(
        matches!(result, Err(CodexErr::UnsupportedOperation(message)) if message.contains("residency transition"))
    );
    assert!(
        !residency.try_reserve_pending_slot(1),
        "timeout cannot release another shutdown owner's occupancy"
    );
    assert!(
        residency
            .state
            .lock()
            .unwrap()
            .evicting
            .contains_key(&child.thread_id)
    );
    drop(claim);
    assert!(
        residency
            .state
            .lock()
            .unwrap()
            .residents
            .contains(&child.thread_id)
    );
}
#[tokio::test]
async fn ma007_cleanup_lineage_reaches_grandchild_through_an_evicted_parent() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let home = tempfile::tempdir().unwrap();
    config.codex_home = home.path().to_path_buf().try_into().unwrap();
    config.cwd = home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager.start_thread(config.clone()).await.unwrap();
    let control = manager.agent_control();
    let state = control.upgrade().unwrap();
    let parent_slot = control
        .reserve_v2_residency_slot(&state, &config, None)
        .await
        .unwrap();
    let parent =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "parent").await;
    parent_slot.commit(parent.thread_id);
    let child_slot = control
        .reserve_v2_residency_slot(&state, &config, None)
        .await
        .unwrap();
    let child = spawn_v2_subagent(
        &control,
        &state,
        config.clone(),
        parent.thread_id,
        "grandchild",
    )
    .await;
    child_slot.commit(child.thread_id);
    mark_thread_interrupted(parent.thread.as_ref()).await;
    let replacement = control
        .reserve_v2_residency_slot(&state, &config, None)
        .await
        .unwrap();
    assert!(manager.get_thread(parent.thread_id).await.is_err());
    assert!(manager.get_thread(child.thread_id).await.is_ok());
    let descendants = control
        .live_thread_spawn_descendants(root.thread_id)
        .await
        .unwrap();
    assert!(
        descendants.contains(&child.thread_id),
        "cleanup must traverse retained ancestry, not only loaded parent edges"
    );
    drop(replacement);
    control.state.release_spawned_thread(parent.thread_id);
    assert!(
        control
            .state
            .known_spawn_edges()
            .contains(&(root.thread_id, parent.thread_id)),
        "a live descendant still needs the released parent's ancestry"
    );
    child.thread.shutdown_and_wait().await.unwrap();
    manager.remove_thread(&child.thread_id).await;
    control.state.release_spawned_thread(child.thread_id);
    assert!(
        !control
            .state
            .known_spawn_edges()
            .iter()
            .any(|(_, id)| *id == parent.thread_id || *id == child.thread_id),
        "closing the last descendant prunes retired ancestry"
    );
}

#[tokio::test]
async fn ma006_durable_binding_reattaches_a_missing_registry_identity_without_reloading() {
    use codex_agent_task_store::*;
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let home = tempfile::tempdir().unwrap();
    config.codex_home = home.path().to_path_buf().try_into().unwrap();
    config.cwd = home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager.start_thread(config.clone()).await.unwrap();
    let control = manager.agent_control();
    let state = control.upgrade().unwrap();
    let child =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "restored").await;
    // Rehydration consults durable session-tree provenance, not just a task binding.
    child.thread.ensure_rollout_materialized().await;
    child.thread.flush_rollout().await.unwrap();
    let source = child.thread.config_snapshot().await.session_source;
    control
        .persist_thread_spawn_edge_for_source(child.thread.as_ref(), child.thread_id, Some(&source))
        .await
        .unwrap();
    let coordinator = control.task_coordinator();
    let root_id = control.session_id().to_string();
    coordinator
        .initialize_for_workspace_coordination(
            None,
            home.path().join("state"),
            config.model_provider_id.clone(),
            root_id.clone(),
        )
        .await
        .unwrap();
    let (assignment, attempt) = coordinator
        .create_assignment(
            home.path(),
            AssignmentDraft {
                root_session_id: root_id,
                admission_origin: AssignmentAdmissionOrigin::Typed,
                role: AgentRole::Worker,
                capability_profile: CapabilityProfile::ScopedSourceWrite,
                objective: "reattach".into(),
                acceptance_criteria: vec![AcceptanceCriterion {
                    evidence_policy: None,
                    id: "done".into(),
                    text: "done".into(),
                }],
                read_scope: Vec::new(),
                write_scope: Vec::new(),
                stop_condition: "stop".into(),
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
    coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: "/root/restored".into(),
            task_name: "restored".into(),
            thread_id: Some(child.thread_id.to_string()),
        })
        .await
        .unwrap();
    control.state.release_spawned_thread(child.thread_id);
    assert!(
        control
            .state
            .agent_metadata_for_thread(child.thread_id)
            .is_none()
    );
    let resolved = control
        .resolve_agent_reference(root.thread_id, &SessionSource::Cli, "/root/restored")
        .await
        .unwrap();
    assert_eq!(resolved, child.thread_id);
    assert!(
        Arc::ptr_eq(&manager.get_thread(resolved).await.unwrap(), &child.thread),
        "restoring addressability must borrow the existing runtime"
    );
}
#[tokio::test]
async fn unloaded_parent_keeps_bounded_queue_only_completion_without_waking() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let home = tempfile::tempdir().unwrap();
    config.codex_home = home.path().to_path_buf().try_into().unwrap();
    config.cwd = home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager.start_thread(config.clone()).await.unwrap();
    let control = manager.agent_control();
    let state = control.upgrade().unwrap();
    let slot = control
        .reserve_v2_residency_slot(&state, &config, None)
        .await
        .unwrap();
    let parent =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "parent").await;
    slot.commit(parent.thread_id);
    let slot = control
        .reserve_v2_residency_slot(&state, &config, None)
        .await
        .unwrap();
    let child =
        spawn_v2_subagent(&control, &state, config.clone(), parent.thread_id, "child").await;
    slot.commit(child.thread_id);
    mark_thread_completed(&parent.thread).await;
    // Deliver the child's completion below, after its parent is actually cold.
    let original_queue = Arc::clone(&parent.thread.codex.session.input_queue);
    let replacement = control
        .reserve_v2_residency_slot(&state, &config, Some(child.thread_id))
        .await
        .unwrap();
    drop(replacement);
    assert!(manager.get_thread(parent.thread_id).await.is_err());
    let mail = crate::session_prefix::inter_agent_completion_communication(
        AgentPath::root(),
        "/root/parent/child".parse().unwrap(),
        child.thread_id,
        &crate::agent::AgentStatus::Errored("validation failed".into()),
        Some("attempt-1"),
    )
    .unwrap();
    let context = crate::agent_communication::AgentCommunicationContext::new(
        crate::agent_communication::AgentCommunicationKind::Result,
        child.thread_id,
    );
    for _ in 0..2 {
        control
            .send_inter_agent_communication(parent.thread_id, mail.clone(), context.clone())
            .await
            .unwrap();
    }
    assert!(
        manager.get_thread(parent.thread_id).await.is_err(),
        "queue-only completion must not load a runtime"
    );
    drop(parent.thread);
    control
        .ensure_v2_agent_loaded(config, parent.thread_id)
        .await
        .unwrap();
    let resumed = manager.get_thread(parent.thread_id).await.unwrap();
    assert!(Arc::ptr_eq(
        &original_queue,
        &resumed.codex.session.input_queue
    ));
    assert!(resumed.codex.session.active_turn.lock().await.is_none());
    let messages = resumed
        .codex
        .session
        .input_queue
        .take_pending_for_state(None, false)
        .await;
    assert_eq!(
        messages.len(),
        1,
        "existing mailbox deduplication survives reload"
    );
    assert!(
        matches!(&messages[0], crate::session::TurnInput::InterAgentCommunication(found) if found == &mail)
    );
    control.close_agent(parent.thread_id).await.unwrap();
    assert!(
        control
            .send_inter_agent_communication(parent.thread_id, mail, context)
            .await
            .is_err()
    );
    assert!(control.retained_input_queue(parent.thread_id).is_none());
}

#[tokio::test]
async fn eviction_pin_fences_admission_without_blocking_owned_shutdown() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let home = tempfile::tempdir().unwrap();
    config.codex_home = home.path().to_path_buf().try_into().unwrap();
    config.cwd = home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager.start_thread(config.clone()).await.unwrap();
    let control = manager.agent_control();
    let state = control.upgrade().unwrap();
    let child = spawn_v2_subagent(&control, &state, config, root.thread_id, "worker").await;
    mark_thread_completed(&child.thread).await;
    let pin = child.thread.try_pin_for_eviction().unwrap();
    let message = codex_protocol::protocol::InterAgentCommunication::new(
        AgentPath::root(),
        "/root/worker".parse().unwrap(),
        Vec::new(),
        "follow-up".into(),
        true,
    );
    let mut delivery = Box::pin(child.thread.submit_with_preacquired_execution_guard(
        codex_protocol::protocol::Op::InterAgentCommunication {
            communication: message,
        },
        None,
    ));
    assert!(futures::poll!(delivery.as_mut()).is_pending());
    child.thread.request_shutdown().await.unwrap();
    child.thread.wait_until_drained().await.unwrap();
    drop(pin);
    assert!(
        delivery.await.is_err(),
        "stale selection must not acknowledge input into a dead runtime"
    );
}
