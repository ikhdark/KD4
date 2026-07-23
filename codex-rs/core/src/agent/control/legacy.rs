use super::*;
use futures::StreamExt;
use std::collections::HashSet;
use std::future::Future;

const AGENT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const AGENT_TREE_SHUTDOWN_CONCURRENCY: usize = 8;

impl AgentControl {
    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        self.shutdown_live_agent_with_timeout(agent_id, AGENT_SHUTDOWN_TIMEOUT)
            .await
    }

    pub(crate) async fn shutdown_live_agent_with_timeout(
        &self,
        agent_id: ThreadId,
        shutdown_timeout: std::time::Duration,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let Ok(thread) = state.get_thread(agent_id).await else {
            let result = state.send_op(agent_id, Op::Shutdown {}).await;
            let _ = state.remove_thread(&agent_id).await;
            self.forget_v2_residency(agent_id);
            self.state.release_spawned_thread(agent_id);
            return result;
        };

        thread.codex.session.ensure_rollout_materialized().await;
        let flush_error: Option<CodexErr> = thread
            .codex
            .session
            .flush_rollout()
            .await
            .err()
            .map(Into::into);
        let shutdown_result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
            Ok(String::new())
        } else {
            state.send_op(agent_id, Op::Shutdown {}).await
        };
        if tokio::time::timeout(shutdown_timeout, thread.wait_until_terminated())
            .await
            .is_err()
        {
            let cleanup_control = self.clone();
            let cleanup_state = Arc::clone(&state);
            let cleanup_thread = Arc::clone(&thread);
            tokio::spawn(async move {
                cleanup_thread.wait_until_terminated().await;
                if cleanup_state
                    .remove_thread_if_same(&agent_id, &cleanup_thread)
                    .await
                {
                    cleanup_control.forget_v2_residency(agent_id);
                    cleanup_control.state.release_spawned_thread(agent_id);
                }
            });

            let mut details = Vec::new();
            if let Some(err) = flush_error.as_ref() {
                details.push(format!("rollout flush failed: {err}"));
            }
            if let Err(err) = shutdown_result.as_ref() {
                details.push(format!("shutdown submission failed: {err}"));
            }
            let details = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join("; "))
            };
            return Err(CodexErr::Fatal(format!(
                "timed out waiting for agent {agent_id} to terminate after {shutdown_timeout:?}{details}"
            )));
        }

        if state.remove_thread_if_same(&agent_id, &thread).await {
            self.forget_v2_residency(agent_id);
            self.state.release_spawned_thread(agent_id);
        }

        match (flush_error, shutdown_result) {
            (None, result) => result,
            (Some(flush_error), Ok(_)) => Err(flush_error),
            (Some(flush_error), Err(shutdown_error)) => Err(CodexErr::Fatal(format!(
                "agent {agent_id} terminated after rollout flush failed ({flush_error}) and shutdown submission failed ({shutdown_error})"
            ))),
        }
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let known_agent = self.state.agent_metadata_for_thread(agent_id).is_some();
        let persistence_error = match state.get_thread(agent_id).await {
            Ok(thread) => {
                if !thread.config_snapshot().await.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                    Some(err.to_string())
                } else {
                    None
                }
            }
            Err(CodexErr::ThreadNotFound(_)) if known_agent => {
                if let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist stale thread-spawn edge status for {agent_id}: {err}");
                    Some(err.to_string())
                } else {
                    None
                }
            }
            Err(CodexErr::ThreadNotFound(_)) => None,
            Err(err) => {
                warn!("failed to inspect agent before close {agent_id}: {err}");
                Some(format!(
                    "failed to inspect agent before persisting closure: {err}"
                ))
            }
        };
        let shutdown_result = match Box::pin(self.shutdown_agent_tree(agent_id)).await {
            Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) if known_agent => {
                Ok(String::new())
            }
            result => result,
        };
        match (persistence_error, shutdown_result) {
            (None, result) => result,
            (Some(persistence_error), Ok(_)) => Err(CodexErr::Fatal(format!(
                "agent {agent_id} shut down, but its closed spawn-edge status was not persisted: {persistence_error}"
            ))),
            (Some(persistence_error), Err(shutdown_error)) => Err(CodexErr::Fatal(format!(
                "agent {agent_id} close was only partially successful: failed to persist closed spawn-edge status ({persistence_error}); shutdown failed ({shutdown_error})"
            ))),
        }
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let mut closing_guard = self.state.begin_closing_agent_tree(agent_id);
        let mut known_thread_ids = HashSet::from([agent_id]);
        let mut descendant_ids = Vec::new();
        loop {
            let snapshot = self.live_thread_spawn_descendants(agent_id).await?;
            let newly_discovered = snapshot
                .into_iter()
                .filter(|thread_id| known_thread_ids.insert(*thread_id))
                .collect::<Vec<_>>();
            if newly_discovered.is_empty() {
                break;
            }
            closing_guard.mark_threads(newly_discovered.iter().copied());
            descendant_ids.extend(newly_discovered);
        }

        let mut shutdown_ids = Vec::with_capacity(descendant_ids.len().saturating_add(1));
        shutdown_ids.push(agent_id);
        shutdown_ids.extend(descendant_ids);

        let state = self.upgrade()?;
        let mut closing_threads = Vec::new();
        for thread_id in shutdown_ids.iter().copied() {
            if let Ok(thread) = state.get_thread(thread_id).await {
                closing_threads.push(thread);
            }
        }

        let mut shutdown_results = run_tree_shutdowns(&shutdown_ids, |thread_id| {
            self.shutdown_live_agent(thread_id)
        })
        .await
        .into_iter();
        let mut result = shutdown_results
            .next()
            .map(|(_, result)| result)
            .unwrap_or_else(|| Ok(String::new()));
        for (descendant_id, descendant_result) in shutdown_results {
            match descendant_result {
                Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {}
                Err(err)
                    if matches!(
                        &result,
                        Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied)
                    ) =>
                {
                    result = Err(err);
                }
                Err(err) => {
                    warn!(
                        "additional failure while shutting down descendant agent {descendant_id}: {err}"
                    );
                }
            }
        }
        tokio::spawn(async move {
            for thread in closing_threads {
                thread.wait_until_terminated().await;
            }
            drop(closing_guard);
        });
        result
    }
}

async fn run_tree_shutdowns<F, Fut>(
    shutdown_ids: &[ThreadId],
    shutdown: F,
) -> Vec<(ThreadId, CodexResult<String>)>
where
    F: Fn(ThreadId) -> Fut,
    Fut: Future<Output = CodexResult<String>>,
{
    let mut results = futures::stream::iter(shutdown_ids.iter().copied().enumerate())
        .map(|(index, thread_id)| {
            let shutdown = shutdown(thread_id);
            async move { (index, thread_id, shutdown.await) }
        })
        .buffer_unordered(AGENT_TREE_SHUTDOWN_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    results.sort_by_key(|(index, _, _)| *index);
    results
        .into_iter()
        .map(|(_, thread_id, result)| (thread_id, result))
        .collect()
}

#[cfg(test)]
#[path = "legacy_tests.rs"]
mod tests;
