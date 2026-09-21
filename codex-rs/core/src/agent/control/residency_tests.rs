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
use codex_protocol::protocol::RolloutLine;
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
                memory_citation: None,
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
    let history = std::fs::read_to_string(&rollout_path).expect("read persisted history");
    let buffered_messages = history
        .lines()
        .map(|line| serde_json::from_str::<RolloutLine>(line).expect("valid rollout line"))
        .filter(|line| {
            matches!(&line.item, RolloutItem::EventMsg(EventMsg::AgentMessage(message))
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
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker_1").await;
    first_slot.commit(first.thread_id);
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
    control
        .ensure_v2_agent_loaded(config.clone(), first.thread_id)
        .await
        .expect("registered interrupted history can reload");
    assert!(manager.get_thread(first.thread_id).await.is_ok());
    // A completed child follows the same reload boundary, evicting the interrupted child.
    let reloaded = manager.get_thread(first.thread_id).await.unwrap();
    mark_thread_completed(reloaded.as_ref()).await;
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
    state
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
            1,
            Some(AgentPath::root().join(label).expect("child path")),
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
    assert!(
        !graph
            .list_thread_spawn_children(
                root.thread_id,
                Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
            )
            .await
            .expect("read open edges before publication")
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
