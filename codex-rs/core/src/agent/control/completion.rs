//! Selected-child completion waits over the lifecycle owner's durable stream.
//! No tasks are spawned: dropping the future unregisters the existing store
//! subscription. Capture the stream position before checking exact attempts,
//! then recheck only selected attempts named by committed events (or retention
//! loss). A follow-up cannot overwrite the terminal result being awaited.

use super::AgentControl;
use codex_agent_task_store::AgentTaskBinding;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::StoreResult;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionMode {
    #[default]
    Any,
    All,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChildCompletionWait {
    pub(crate) reason: &'static str,
    mode: CompletionMode,
    outcomes: Vec<Value>,
    pending: Vec<AgentTaskBinding>,
    // A sealed producer receipt is not an integration/review verdict.
    acceptance: &'static str,
}

impl AgentControl {
    pub(crate) async fn wait_for_child_completions(
        &self,
        caller: &AgentPath,
        children: &[ThreadId],
        mode: CompletionMode,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> StoreResult<ChildCompletionWait> {
        let coordinator = self.task_coordinator();
        let root = coordinator.root_session_id().ok_or_else(|| {
            StoreError::InvalidAssignment(
                "completion wait requires durable task coordination".into(),
            )
        })?;
        let store = coordinator.store().ok_or_else(|| {
            StoreError::InvalidAssignment("completion wait task store is unavailable".into())
        })?;
        let bindings = coordinator.bindings_for_recovery();
        let prefix = format!("{caller}/");
        let mut seen = HashSet::new();
        let mut pending = Vec::new();
        for child in children {
            if !seen.insert(*child) {
                continue;
            }
            let id = child.to_string();
            let binding = bindings
                .iter()
                .find(|binding| {
                    binding.thread_id.as_deref() == Some(id.as_str())
                        && binding.root_session_id == root
                        && binding.agent_path.starts_with(&prefix)
                })
                .ok_or_else(|| {
                    StoreError::InvalidAssignment(format!(
                        "completion target {child} is not a bound descendant of {caller}"
                    ))
                })?;
            pending.push(binding.clone());
        }
        if pending.is_empty() {
            return Err(StoreError::InvalidAssignment(
                "completion targets cannot be empty".into(),
            ));
        }
        let mut result = ChildCompletionWait {
            reason: "completed",
            mode,
            outcomes: Vec::new(),
            pending,
            acceptance: "not_evaluated",
        };
        let work = async {
            // Snapshot before state inspection: events committed during inspection
            // are replayed by wait_for_wake_events, including before registration.
            let mut cursor = store.current_wake_cursor(&root).await?;
            let mut changed = result
                .pending
                .iter()
                .map(|binding| binding.assignment_id)
                .collect::<HashSet<_>>();
            loop {
                let mut index = 0;
                while index < result.pending.len() {
                    let binding = &result.pending[index];
                    if !changed.contains(&binding.assignment_id) {
                        index += 1;
                        continue;
                    }
                    let (attempt, receipt) = store
                        .get_agent_attempt(binding.assignment_id, binding.attempt_id)
                        .await?;
                    if attempt.state == AttemptState::Active {
                        index += 1;
                    } else {
                        result.outcomes.push(json!({
                            "task_name": binding.agent_path, "thread_id": binding.thread_id,
                            "assignment_id": binding.assignment_id, "attempt_id": binding.attempt_id,
                            "state": attempt.state, "receipt_status": receipt.as_ref().map(|receipt| receipt.status),
                            "receipt_available": receipt.is_some(),
                            "result_recovery": {"tool": "get_agent_task", "arguments": {
                                "assignment_id": binding.assignment_id, "attempt_id": binding.attempt_id
                            }}
                        }));
                        // No await separates publication from removing the
                        // obligation; cancellation keeps every partial outcome.
                        result.pending.remove(index);
                        if matches!(mode, CompletionMode::Any) {
                            return Ok::<(), StoreError>(());
                        }
                    }
                }
                if result.pending.is_empty()
                    || matches!(mode, CompletionMode::Any) && !result.outcomes.is_empty()
                {
                    return Ok::<(), StoreError>(());
                }
                let wake = store.wait_for_wake_events(root.clone(), cursor).await?;
                cursor = wake.latest_event_id;
                changed = if wake.lost_to_retention_count > 0 {
                    result
                        .pending
                        .iter()
                        .map(|binding| binding.assignment_id)
                        .collect()
                } else {
                    wake.updated_agents
                        .iter()
                        .map(|event| event.assignment_id)
                        .collect()
                };
            }
        };
        let timeout = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => result.reason = "cancelled",
            _ = timeout => result.reason = "timed_out",
            outcome = work => outcome?,
        }
        Ok(result)
    }
}
