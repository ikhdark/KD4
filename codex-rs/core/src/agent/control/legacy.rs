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
        let control = self.clone();
        // The owner survives both a foreground timeout and caller cancellation, including
        // cancellation during persistence or shutdown submission.
        let shutdown =
            tokio::spawn(async move { control.shutdown_live_agent_to_completion(agent_id).await });
        match tokio::time::timeout(shutdown_timeout, shutdown).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(CodexErr::Fatal(format!(
                "agent {agent_id} shutdown task failed: {error}"
            ))),
            Err(_) => Err(CodexErr::Fatal(format!(
                "timed out waiting for agent {agent_id} to terminate after {shutdown_timeout:?}"
            ))),
        }
    }

    async fn shutdown_live_agent_to_completion(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        // A missing observation authorizes no retirement: a cold loader may already
        // own this identity, or publish a replacement immediately after the lookup.
        let thread = state.get_thread(agent_id).await?;

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
            state
                .send_op_to_instance(agent_id, &thread, Op::Shutdown {})
                .await
        };
        thread.wait_until_drained().await?;

        // Shutdown acknowledgement is bounded, but accepted terminal work can
        // outlive it. Retain the registered worker until that owner drains too.
        thread.codex.session.terminal_tasks.wait().await;
        thread
            .codex
            .session
            .wait_for_ordered_history_commits()
            .await;

        state
            .remove_thread_if_same_with_cleanup(&agent_id, &thread, || {
                self.forget_v2_residency(agent_id);
                self.state.release_spawned_thread(agent_id);
            })
            .await;

        match (flush_error, shutdown_result) {
            (None, result) => result,
            (Some(flush_error), Ok(_)) => Err(flush_error),
            (Some(flush_error), Err(shutdown_error)) => Err(CodexErr::Fatal(format!(
                "agent {agent_id} terminated after rollout flush failed ({flush_error}) and shutdown submission failed ({shutdown_error})"
            ))),
        }
    }

    /// After a shutdown observation timeout, wait for the existing owners instead
    /// of starting duplicate shutdown tasks. Retained registry edges include a
    /// descendant even if its direct parent has already been unloaded.
    pub(crate) async fn wait_for_agent_tree_termination(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<()> {
        let mut ids = self.live_thread_spawn_descendants(agent_id).await?;
        ids.push(agent_id);
        let state = self.upgrade()?;
        for id in ids {
            if let Ok(thread) = state.get_thread(id).await {
                thread.wait_until_terminated().await;
                thread.codex.session.terminal_tasks.wait().await;
                thread
                    .codex
                    .session
                    .wait_for_ordered_history_commits()
                    .await;
            }
        }
        Ok(())
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let control = self.clone();
        // Own the entire transition before its first durable effect, not just shutdown.
        tokio::spawn(async move { control.close_agent_to_completion(agent_id).await })
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("agent {agent_id} close task failed: {error}"))
            })?
    }

    async fn close_agent_to_completion(&self, agent_id: ThreadId) -> CodexResult<String> {
        let _closing_guard = self.state.begin_closing_agent_tree(agent_id);
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
        let control = self.clone();
        tokio::spawn(async move { control.shutdown_agent_tree_to_completion(agent_id).await })
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "agent tree {agent_id} shutdown task failed: {error}"
                ))
            })?
    }

    async fn shutdown_agent_tree_to_completion(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let mut closing_guard = self.state.begin_closing_agent_tree(agent_id);
        let mut topology_error = None;
        let mut known_thread_ids = HashSet::from([agent_id]);
        let mut descendant_ids = Vec::new();
        loop {
            let snapshot = match self.live_thread_spawn_descendants(agent_id).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    // A broken graph must not prevent known live work from stopping,
                    // nor may a partial shutdown be reported as complete.
                    topology_error = Some(error);
                    state
                        .retained_agent_subtree(agent_id, self.state.known_spawn_edges())
                        .await?
                        .into_iter()
                        .skip(1)
                        .collect()
                }
            };
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
        if let Some(error) = topology_error
            && matches!(
                result,
                Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied)
            )
        {
            result = Err(error);
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
