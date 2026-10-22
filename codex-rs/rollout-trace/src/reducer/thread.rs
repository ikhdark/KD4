//! Thread and turn reduction.
//!
//! Threads are the container that every other reducer module links into. This
//! module owns the identity metadata parsing as well, so the central dispatcher
//! does not need to know the shape of multi-agent session-source payloads.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde_json::Value;

use super::TraceReducer;
use super::tool::spawn_edge_id;
use crate::model::AgentOrigin;
use crate::model::AgentThread;
use crate::model::CodexTurn;
use crate::model::CodexTurnId;
use crate::model::ExecutionStatus;
use crate::model::ExecutionWindow;
use crate::model::RolloutStatus;
use crate::payload::RawPayloadRef;
use crate::raw_event::RawEventSeq;

impl TraceReducer {
    /// Inserts a thread and derives its multi-agent identity from optional metadata.
    ///
    /// The raw event carries a denormalized agent path; when v2 subagent metadata is
    /// present, that metadata is authoritative because it also drives spawn edges and task names.
    pub(super) fn start_thread(
        &mut self,
        seq: RawEventSeq,
        wall_time_unix_ms: i64,
        thread_id: String,
        agent_path: String,
        metadata_payload: Option<RawPayloadRef>,
    ) -> Result<()> {
        if self.rollout.threads.contains_key(&thread_id) {
            bail!("duplicate thread start for {thread_id}");
        }

        let metadata = metadata_payload
            .as_ref()
            .map(|payload| self.thread_started_metadata(payload))
            .transpose()?;
        let spawn = metadata
            .as_ref()
            .map(ThreadStartedMetadata::thread_spawn)
            .transpose()?
            .flatten();
        // The v2 SessionSource is the authoritative child identity record.
        // Prefer its nested agent_path over the denormalized event field so
        // task derivation and the spawn edge are based on the same metadata.
        let agent_path = spawn
            .as_ref()
            .and_then(|spawn| spawn.agent_path.clone())
            .or_else(|| {
                metadata
                    .as_ref()
                    .and_then(|metadata| metadata.agent_path.clone())
            })
            .unwrap_or(agent_path);
        let nickname = metadata
            .as_ref()
            .and_then(|metadata| metadata.nickname.clone());
        let default_model = metadata
            .as_ref()
            .and_then(|metadata| metadata.model.clone());
        let origin = if let Some(spawn) = spawn {
            let edge_id = spawn_edge_id(&spawn.parent_thread_id, &thread_id);
            let task_name = spawn
                .task_name
                .clone()
                .unwrap_or_else(|| task_name_from_agent_path(&agent_path));
            let agent_role = spawn.agent_role.clone().unwrap_or_default();

            AgentOrigin::Spawned {
                parent_thread_id: spawn.parent_thread_id,
                spawn_edge_id: edge_id,
                task_name,
                agent_role,
            }
        } else {
            AgentOrigin::Root
        };

        self.rollout.threads.insert(
            thread_id.clone(),
            AgentThread {
                thread_id,
                agent_path,
                nickname,
                origin,
                execution: ExecutionWindow {
                    started_at_unix_ms: wall_time_unix_ms,
                    started_seq: seq,
                    ended_at_unix_ms: None,
                    ended_seq: None,
                    status: ExecutionStatus::Running,
                },
                previous_executions: Vec::new(),
                history_before_trace: false,
                default_model,
                conversation_item_ids: Vec::new(),
            },
        );
        Ok(())
    }

    /// Reopens a closed identity, or imports only the newly observed interval of
    /// a historical child. Resuming never fabricates a spawn/delivery edge.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resume_thread(
        &mut self,
        seq: RawEventSeq,
        wall_time_unix_ms: i64,
        thread_id: String,
        parent_thread_id: String,
        agent_path: String,
        metadata_payload: Option<RawPayloadRef>,
    ) -> Result<()> {
        self.thread_mut(&parent_thread_id)?;
        let metadata = metadata_payload
            .as_ref()
            .map(|payload| self.thread_started_metadata(payload))
            .transpose()?;
        let spawn = metadata
            .as_ref()
            .map(ThreadStartedMetadata::thread_spawn)
            .transpose()?
            .flatten();
        if let Some(spawn) = &spawn {
            anyhow::ensure!(
                spawn.parent_thread_id == parent_thread_id,
                "resumed thread parent mismatch"
            );
            anyhow::ensure!(
                spawn
                    .agent_path
                    .as_ref()
                    .is_none_or(|path| path == &agent_path),
                "resumed thread path mismatch"
            );
        }
        if !self.rollout.threads.contains_key(&thread_id) {
            self.start_thread(
                seq,
                wall_time_unix_ms,
                thread_id.clone(),
                agent_path.clone(),
                metadata_payload,
            )?;
            let thread = self.thread_mut(&thread_id)?;
            // Parent linkage remains known even when optional metadata capture failed.
            thread.origin = AgentOrigin::Spawned {
                spawn_edge_id: spawn_edge_id(&parent_thread_id, &thread_id),
                parent_thread_id,
                task_name: spawn
                    .as_ref()
                    .and_then(|spawn| spawn.task_name.clone())
                    .unwrap_or_else(|| task_name_from_agent_path(&agent_path)),
                agent_role: spawn.and_then(|spawn| spawn.agent_role).unwrap_or_default(),
            };
            thread.history_before_trace = true;
            return Ok(());
        }
        let thread = self.thread_mut(&thread_id)?;
        anyhow::ensure!(
            thread.agent_path == agent_path,
            "resumed thread path mismatch"
        );
        anyhow::ensure!(
            matches!(&thread.origin, AgentOrigin::Spawned { parent_thread_id: existing, .. } if existing == &parent_thread_id),
            "resumed thread parent mismatch"
        );
        anyhow::ensure!(
            thread.execution.ended_seq.is_some(),
            "resumed thread is still active: {thread_id}"
        );
        let previous = std::mem::replace(
            &mut thread.execution,
            ExecutionWindow {
                started_at_unix_ms: wall_time_unix_ms,
                started_seq: seq,
                ended_at_unix_ms: None,
                ended_seq: None,
                status: ExecutionStatus::Running,
            },
        );
        thread.previous_executions.push(previous);
        if let Some(metadata) = metadata {
            thread.default_model = metadata.model;
            thread.nickname = metadata.nickname;
        }
        Ok(())
    }

    /// Marks a thread terminal without treating child shutdown as rollout completion.
    pub(super) fn end_thread(
        &mut self,
        seq: RawEventSeq,
        wall_time_unix_ms: i64,
        thread_id: String,
        status: RolloutStatus,
    ) -> Result<()> {
        let thread = self.thread_mut(&thread_id)?;
        thread.execution.ended_at_unix_ms = Some(wall_time_unix_ms);
        thread.execution.ended_seq = Some(seq);
        thread.execution.status = match status {
            RolloutStatus::Running => ExecutionStatus::Running,
            RolloutStatus::Completed => ExecutionStatus::Completed,
            RolloutStatus::Failed => ExecutionStatus::Failed,
            RolloutStatus::Aborted => ExecutionStatus::Aborted,
        };
        Ok(())
    }

    /// Starts a Codex turn inside an existing thread.
    pub(super) fn start_codex_turn(
        &mut self,
        seq: RawEventSeq,
        wall_time_unix_ms: i64,
        codex_turn_id: CodexTurnId,
        thread_id: String,
    ) -> Result<()> {
        if self.rollout.codex_turns.contains_key(&codex_turn_id) {
            bail!("duplicate codex turn start for {codex_turn_id}");
        }

        self.thread_mut(&thread_id)?;

        self.rollout.codex_turns.insert(
            codex_turn_id.clone(),
            CodexTurn {
                codex_turn_id,
                thread_id,
                execution: ExecutionWindow {
                    started_at_unix_ms: wall_time_unix_ms,
                    started_seq: seq,
                    ended_at_unix_ms: None,
                    ended_seq: None,
                    status: ExecutionStatus::Running,
                },
                input_item_ids: Vec::new(),
            },
        );
        Ok(())
    }

    /// Marks a Codex turn terminal and validates any thread id carried by the raw event.
    pub(super) fn end_codex_turn(
        &mut self,
        seq: RawEventSeq,
        wall_time_unix_ms: i64,
        thread_id: Option<String>,
        codex_turn_id: CodexTurnId,
        status: ExecutionStatus,
    ) -> Result<()> {
        if let Some(event_thread_id) = thread_id.as_deref()
            && let Some(turn) = self.rollout.codex_turns.get(&codex_turn_id)
            && turn.thread_id != event_thread_id
        {
            bail!(
                "codex turn end for {codex_turn_id} used thread {event_thread_id}, \
                 but the turn belongs to {}",
                turn.thread_id
            );
        }

        let Some(turn) = self.rollout.codex_turns.get_mut(&codex_turn_id) else {
            bail!("codex turn end referenced unknown turn {codex_turn_id}");
        };
        turn.execution.ended_at_unix_ms = Some(wall_time_unix_ms);
        turn.execution.ended_seq = Some(seq);
        turn.execution.status = status.clone();
        self.close_code_cell_owners_for_turn_end(seq, wall_time_unix_ms, &codex_turn_id, &status)?;
        self.close_running_inference_calls_for_turn_end(
            seq,
            wall_time_unix_ms,
            &codex_turn_id,
            &status,
        );
        self.close_running_compaction_requests_for_turn_end(
            seq,
            wall_time_unix_ms,
            &codex_turn_id,
            &status,
        );
        Ok(())
    }

    /// Returns a mutable thread or reports a reducer error tied to the unknown id.
    pub(super) fn thread_mut(&mut self, thread_id: &str) -> Result<&mut AgentThread> {
        self.rollout
            .threads
            .get_mut(thread_id)
            .with_context(|| format!("trace event referenced unknown thread {thread_id}"))
    }

    fn thread_started_metadata(
        &self,
        metadata_payload: &RawPayloadRef,
    ) -> Result<ThreadStartedMetadata> {
        let value = self.read_payload_json(metadata_payload)?;
        serde_json::from_value(value)
            .with_context(|| format!("parse thread metadata {}", metadata_payload.raw_payload_id))
    }
}

#[derive(Deserialize)]
struct ThreadStartedMetadata {
    agent_path: Option<String>,
    task_name: Option<String>,
    nickname: Option<String>,
    agent_role: Option<String>,
    model: Option<String>,
    session_source: Option<Value>,
}

impl ThreadStartedMetadata {
    fn thread_spawn(&self) -> Result<Option<ThreadSpawnMetadata>> {
        let Some(spawn) = self
            .session_source
            .as_ref()
            .and_then(|source| source.get("subagent"))
            .and_then(|subagent| subagent.get("thread_spawn"))
        else {
            return Ok(None);
        };
        let parent_thread_id = spawn
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .context("thread_spawn metadata requires a nonempty string parent_thread_id")?;
        let agent_path = spawn
            .get("agent_path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.agent_path.clone());
        Ok(Some(ThreadSpawnMetadata {
            parent_thread_id: parent_thread_id.to_string(),
            agent_path: agent_path.clone(),
            task_name: spawn
                .get("task_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| self.task_name.clone())
                .or_else(|| agent_path.as_deref().map(task_name_from_agent_path)),
            agent_role: spawn
                .get("agent_role")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| self.agent_role.clone()),
        }))
    }
}

struct ThreadSpawnMetadata {
    parent_thread_id: String,
    agent_path: Option<String>,
    task_name: Option<String>,
    agent_role: Option<String>,
}

fn task_name_from_agent_path(agent_path: &str) -> String {
    agent_path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(agent_path)
        .to_string()
}
