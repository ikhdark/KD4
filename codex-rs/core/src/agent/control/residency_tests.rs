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
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

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
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
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
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("second resident slot should evict the first idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first.thread_id),
        Err(err) => panic!("expected evicted thread to be missing, got {err:?}"),
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second = spawn_v2_subagent(&control, &state, config, root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
}

#[tokio::test]
async fn interrupted_v2_agent_is_lost_after_residency_eviction() {
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
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
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
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);
    mark_thread_completed(second.thread.as_ref()).await;

    let err = control
        .ensure_v2_agent_loaded(config, first.thread_id)
        .await
        .expect_err("evicted interrupted agent should stay lost");
    match err {
        CodexErr::ThreadNotFound(thread_id) => assert_eq!(thread_id, first.thread_id),
        err => panic!("expected ThreadNotFound, got {err:?}"),
    }

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
    match manager.get_thread(first.thread_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first.thread_id),
        Err(err) => panic!("expected evicted thread to be missing, got {err:?}"),
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
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
        .spawn_agent(config, vec![], Some(duplicate_source))
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
}

async fn spawn_v2_subagent(
    control: &AgentControl,
    state: &Arc<ThreadManagerState>,
    config: Config,
    parent_thread_id: ThreadId,
    label: &str,
) -> crate::thread_manager::NewThread {
    state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::Other(label.to_string())),
            Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("spawn v2 subagent")
}

async fn mark_thread_completed(thread: &CodexThread) {
    let turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                last_agent_message: Some("done".to_string()),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                completion: None,
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
