use std::future::Future;
use std::pin::Pin;

use codex_protocol::ThreadId;

use crate::AgentGraphStoreResult;
use crate::ThreadSpawnEdgeStatus;

/// Future returned by [`AgentGraphStore`] operations.
pub type AgentGraphStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = AgentGraphStoreResult<T>> + Send + 'a>>;

/// Storage-neutral boundary for persisted thread-spawn parent/child topology.
///
/// Implementations are expected to return stable ordering for list methods so callers can merge
/// persisted graph state with live in-memory state without introducing nondeterministic output.
pub trait AgentGraphStore: Send + Sync {
    /// Reconcile retained/live edges with current durable parent authority, then
    /// return reachable edges from the root. A durable incoming edge wins over
    /// a supplied edge for the same child, including Closed/future-status edges.
    /// Implementations must terminate on legacy cycles and avoid per-node I/O.
    fn list_reconciled_thread_spawn_edges(
        &self,
        root_thread_id: ThreadId,
        live_edges: Vec<(ThreadId, ThreadId)>,
    ) -> AgentGraphStoreFuture<'_, Vec<(ThreadId, ThreadId)>>;

    /// Insert or replace the directional parent/child edge for a spawned thread.
    ///
    /// `child_thread_id` has at most one persisted parent, and persisted edges must remain
    /// acyclic. Re-inserting the same child should update both the parent and status to match the
    /// supplied values, unless the requested reparenting would create a cycle.
    ///
    /// Topology validation and replacement must preserve these invariants under concurrent writes.
    /// Rejected topology changes return `AgentGraphStoreError::InvalidRequest` and leave existing
    /// relationships, including their statuses, unchanged.
    fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()>;

    /// Update the persisted lifecycle status of a spawned thread's incoming edge.
    ///
    /// Implementations should treat missing children as a successful no-op.
    fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()>;

    /// List direct spawned children of a parent thread.
    ///
    /// When `status_filter` is `Some`, only child edges with that exact status are returned. When
    /// it is `None`, all direct child edges are returned regardless of status, including statuses
    /// that may be added by a future store implementation.
    fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>>;

    /// Check one current incoming edge without requiring a sibling inventory.
    fn has_thread_spawn_child(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, bool> {
        Box::pin(async move {
            Ok(self
                .list_thread_spawn_children(parent_thread_id, Some(status))
                .await?
                .contains(&child_thread_id))
        })
    }

    /// List spawned descendants breadth-first by depth, then by thread id.
    ///
    /// `status_filter` is applied to every traversed edge, not just to the returned descendants.
    /// For example, `Some(Open)` walks only open edges, so descendants under a closed edge are not
    /// included even if their own incoming edge is open. `None` walks and returns every persisted
    /// edge regardless of status.
    fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>>;
}
