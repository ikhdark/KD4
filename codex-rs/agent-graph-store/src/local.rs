use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use std::sync::Arc;

use crate::AgentGraphStore;
use crate::AgentGraphStoreError;
use crate::AgentGraphStoreFuture;
use crate::ThreadSpawnEdgeStatus;

/// SQLite-backed implementation of [`AgentGraphStore`] using an existing state runtime.
#[derive(Clone)]
pub struct LocalAgentGraphStore {
    state_db: Arc<StateRuntime>,
}

impl std::fmt::Debug for LocalAgentGraphStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAgentGraphStore")
            .field("codex_home", &self.state_db.codex_home())
            .finish_non_exhaustive()
    }
}

impl LocalAgentGraphStore {
    /// Create a local graph store from an already-initialized state runtime.
    pub fn new(state_db: Arc<StateRuntime>) -> Self {
        Self { state_db }
    }
}

impl AgentGraphStore for LocalAgentGraphStore {
    fn list_reconciled_thread_spawn_edges(
        &self,
        root_thread_id: ThreadId,
        live_edges: Vec<(ThreadId, ThreadId)>,
    ) -> AgentGraphStoreFuture<'_, Vec<(ThreadId, ThreadId)>> {
        Box::pin(async move {
            self.state_db
                .list_reconciled_thread_spawn_edges(root_thread_id, &live_edges)
                .await
                .map_err(|err| internal_error("reconcile thread-spawn edges", err))
        })
    }

    fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .upsert_thread_spawn_edge(
                    parent_thread_id,
                    child_thread_id,
                    to_state_status(status),
                )
                .await
                .map_err(thread_spawn_edge_write_error)
        })
    }

    fn set_thread_spawn_turn_owner<'a>(
        &'a self,
        parent: ThreadId,
        child: ThreadId,
        turn_id: &'a str,
    ) -> AgentGraphStoreFuture<'a, bool> {
        Box::pin(async move {
            self.state_db.set_thread_spawn_turn_owner(parent, child, turn_id).await
                .map_err(|err| internal_error("record thread-spawn turn owner", err))
        })
    }

    fn list_turn_owned_thread_spawn_descendants<'a>(
        &'a self,
        parent: ThreadId,
        turn_id: &'a str,
    ) -> AgentGraphStoreFuture<'a, Vec<ThreadId>> {
        Box::pin(async move {
            self.state_db.list_turn_owned_thread_spawn_descendants(parent, turn_id).await
                .map_err(|err| internal_error("list turn-owned thread-spawn children", err))
        })
    }

    fn retain_thread_spawn_instruction_snapshot<'a>(
        &'a self,
        parent: ThreadId,
        child: ThreadId,
        snapshot: &'a str,
    ) -> AgentGraphStoreFuture<'a, bool> {
        Box::pin(async move {
            self.state_db.retain_thread_spawn_instruction_snapshot(parent, child, snapshot).await
                .map_err(|err| internal_error("retain inherited instruction snapshot", err))
        })
    }

    fn thread_spawn_instruction_snapshot(&self, child: ThreadId)
        -> AgentGraphStoreFuture<'_, Option<String>> {
        Box::pin(async move {
            self.state_db.thread_spawn_instruction_snapshot(child).await
                .map_err(|err| internal_error("read inherited instruction snapshot", err))
        })
    }

    fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .set_thread_spawn_edge_status(child_thread_id, to_state_status(status))
                .await
                .map_err(|err| internal_error("update thread-spawn edge status", err))
        })
    }

    fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            if let Some(status) = status_filter {
                return self
                    .state_db
                    .list_thread_spawn_children_with_status(
                        parent_thread_id,
                        to_state_status(status),
                    )
                    .await
                    .map_err(|err| internal_error("list thread-spawn children by status", err));
            }

            self.state_db
                .list_thread_spawn_children(parent_thread_id)
                .await
                .map_err(|err| internal_error("list thread-spawn children", err))
        })
    }

    fn has_thread_spawn_child(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, bool> {
        Box::pin(async move {
            self.state_db
                .has_thread_spawn_child(parent_thread_id, child_thread_id, to_state_status(status))
                .await
                .map_err(|err| internal_error("check thread-spawn edge", err))
        })
    }

    fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            match status_filter {
                Some(status) => self
                    .state_db
                    .list_thread_spawn_descendants_with_status(
                        root_thread_id,
                        to_state_status(status),
                    )
                    .await
                    .map_err(|err| internal_error("list thread-spawn descendants by status", err)),
                None => self
                    .state_db
                    .list_thread_spawn_descendants(root_thread_id)
                    .await
                    .map_err(|err| internal_error("list thread-spawn descendants", err)),
            }
        })
    }
}

fn to_state_status(status: ThreadSpawnEdgeStatus) -> codex_state::DirectionalThreadSpawnEdgeStatus {
    match status {
        ThreadSpawnEdgeStatus::Open => codex_state::DirectionalThreadSpawnEdgeStatus::Open,
        ThreadSpawnEdgeStatus::Closed => codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
    }
}

fn thread_spawn_edge_write_error(
    err: codex_state::ThreadSpawnEdgeWriteError,
) -> AgentGraphStoreError {
    match err {
        codex_state::ThreadSpawnEdgeWriteError::InvalidRequest { message } => {
            AgentGraphStoreError::InvalidRequest { message }
        }
        err => internal_error("upsert thread-spawn edge", err),
    }
}

fn internal_error(operation: &'static str, err: impl Into<anyhow::Error>) -> AgentGraphStoreError {
    AgentGraphStoreError::Internal {
        operation,
        source: err.into(),
    }
}

