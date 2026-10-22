use super::*;
use pretty_assertions::assert_eq;
use tokio::sync::Semaphore;

fn spawn_source(parent: ThreadId, depth: i32) -> SessionSource {
    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent,
        depth,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    })
}

struct DelayedCloseGraph {
    inner: Arc<dyn AgentGraphStore>,
    committed: Semaphore,
    release: Semaphore,
}

impl AgentGraphStore for DelayedCloseGraph {
    fn list_reconciled_thread_spawn_edges(
        &self,
        root: ThreadId,
        edges: Vec<(ThreadId, ThreadId)>,
    ) -> AgentGraphStoreFuture<'_, Vec<(ThreadId, ThreadId)>> {
        self.inner.list_reconciled_thread_spawn_edges(root, edges)
    }

    fn upsert_thread_spawn_edge(
        &self,
        parent: ThreadId,
        child: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        self.inner.upsert_thread_spawn_edge(parent, child, status)
    }

    fn set_thread_spawn_edge_status(
        &self,
        child: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.inner
                .set_thread_spawn_edge_status(child, status)
                .await?;
            if status == ThreadSpawnEdgeStatus::Closed {
                self.committed.add_permits(1);
                self.release
                    .acquire()
                    .await
                    .expect("release close acknowledgement")
                    .forget();
            }
            Ok(())
        })
    }

    fn list_thread_spawn_children(
        &self,
        parent: ThreadId,
        status: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        self.inner.list_thread_spawn_children(parent, status)
    }

    fn list_thread_spawn_descendants(
        &self,
        root: ThreadId,
        status: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        self.inner.list_thread_spawn_descendants(root, status)
    }
}

#[tokio::test]
async fn cancelled_close_after_closed_commit_still_shuts_down_the_subtree() {
    let (home, mut config) = test_config().await;
    config.sqlite_home = home.path().join("sqlite");
    let db = init_state_db(&config).await.expect("state db");
    let graph = Arc::new(DelayedCloseGraph {
        inner: Arc::new(codex_agent_graph_store::LocalAgentGraphStore::new(
            db.clone(),
        )),
        committed: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let harness = AgentControlHarness::new_with_config_and_agent_graph_store(
        home,
        config,
        Some(graph.clone()),
    )
    .await;
    let (root, _) = harness.start_thread().await;
    let child = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("child"),
            Some(spawn_source(root, 1)),
        )
        .await
        .unwrap();
    let grandchild = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("grandchild"),
            Some(spawn_source(child, 2)),
        )
        .await
        .unwrap();
    let control = harness.control.clone();
    let close = tokio::spawn(async move { control.close_agent(child).await });
    timeout(Duration::from_secs(5), graph.committed.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(
        db.list_thread_spawn_children_with_status(root, DirectionalThreadSpawnEdgeStatus::Closed)
            .await
            .unwrap(),
        vec![child]
    );
    close.abort();
    assert!(close.await.unwrap_err().is_cancelled());
    graph.release.add_permits(1);
    timeout(Duration::from_secs(5), async {
        while harness.manager.get_thread(child).await.is_ok()
            || harness.manager.get_thread(grandchild).await.is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the independent close owner finishes after cancellation");
    harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
}

#[tokio::test]
async fn subtree_removal_fences_the_actual_session_registry_and_rolls_back_admission() {
    let harness = AgentControlHarness::new().await;
    let (root, root_thread) = harness.start_thread().await;
    let actual_control = root_thread.codex.session.services.agent_control.clone();
    let (guard, ids) = harness
        .manager
        .begin_agent_subtree_removal(root)
        .await
        .unwrap();
    assert_eq!(ids, vec![root]);
    assert!(
        actual_control
            .spawn_agent(
                harness.config.clone(),
                text_input("fenced child"),
                Some(spawn_source(root, 1))
            )
            .await
            .is_err()
    );
    assert!(Arc::ptr_eq(
        &harness.manager.get_thread(root).await.unwrap(),
        &root_thread
    ));
    assert!(
        root_thread.is_running(),
        "precommit admission fencing must not stop healthy work"
    );
    drop(guard);
    let child = actual_control
        .spawn_agent(
            harness.config.clone(),
            text_input("after rollback"),
            Some(spawn_source(root, 1)),
        )
        .await
        .expect("admission restored");
    actual_control.shutdown_agent_tree(root).await.unwrap();
    assert_thread_not_loaded(&harness.manager, child).await;
}

#[tokio::test]
async fn reconciled_subtree_shuts_down_ephemeral_descendant_below_cold_parent() {
    let harness = AgentControlHarness::new().await;
    let (root, _) = harness.start_thread().await;
    let parent = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("parent"),
            Some(spawn_source(root, 1)),
        )
        .await
        .unwrap();
    let mut ephemeral = harness.config.clone();
    ephemeral.ephemeral = true;
    let child = harness
        .control
        .spawn_agent(
            ephemeral,
            text_input("ephemeral child"),
            Some(spawn_source(parent, 2)),
        )
        .await
        .unwrap();
    harness.control.shutdown_live_agent(parent).await.unwrap();
    let ids = harness
        .manager
        .list_agent_subtree_thread_ids(root)
        .await
        .unwrap();
    assert!(ids.contains(&parent));
    assert!(
        ids.contains(&child),
        "mixed durable/live closure must bridge the cold parent"
    );
    harness.control.shutdown_agent_tree(root).await.unwrap();
    assert_thread_not_loaded(&harness.manager, child).await;
    assert_thread_not_loaded(&harness.manager, parent).await;
}

#[tokio::test]
async fn v1_reparent_uses_current_parent_in_runtime_and_stored_projection() {
    let harness = AgentControlHarness::new().await;
    let (old_parent, old_parent_thread) = harness.start_thread().await;
    let (new_parent, new_parent_thread) = harness.start_thread().await;
    // Pin the runtime version before child creation. Feature defaults alone do
    // not create a version receipt; unversioned resumed histories migrate to V2.
    for parent in [&old_parent_thread, &new_parent_thread] {
        assert_eq!(
            parent
                .codex
                .session
                .set_multi_agent_version_if_unset(MultiAgentVersion::V1),
            MultiAgentVersion::V1,
        );
    }
    let child = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("child"),
            Some(spawn_source(old_parent, 1)),
        )
        .await
        .unwrap();
    let old = harness.manager.get_thread(child).await.unwrap();
    assert_eq!(old.multi_agent_version(), Some(MultiAgentVersion::V1));
    old.ensure_rollout_materialized().await;
    old.flush_rollout().await.unwrap();
    harness.control.shutdown_live_agent(child).await.unwrap();
    drop(old);
    harness
        .control
        .resume_agent_from_rollout(harness.config.clone(), child, spawn_source(new_parent, 1))
        .await
        .unwrap();
    let current = harness.manager.get_thread(child).await.unwrap();
    assert_eq!(current.session_source.parent_thread_id(), Some(new_parent));
    assert_eq!(
        current.session_configured().parent_thread_id,
        Some(new_parent)
    );
    let stored = harness
        .control
        .upgrade()
        .unwrap()
        .read_stored_thread(ReadThreadParams {
            thread_id: child,
            include_archived: true,
            include_history: false,
        })
        .await
        .unwrap();
    assert_eq!(stored.parent_thread_id, Some(new_parent));
    assert_eq!(stored.source.parent_thread_id(), Some(new_parent));
    let header = codex_rollout::read_session_meta_line(&current.rollout_path().unwrap())
        .await
        .unwrap();
    assert_eq!(
        header.meta.parent_thread_id,
        Some(old_parent),
        "creation provenance stays immutable"
    );
    harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
}
