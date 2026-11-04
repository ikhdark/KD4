//! Project legacy collaboration events while retaining call ownership and terminal outcomes.

use super::ThreadHistoryBuilder;
use codex_app_server_protocol_types::CollabAgentState;
use codex_app_server_protocol_types::CollabAgentTool;
use codex_app_server_protocol_types::CollabAgentToolCallStatus;
use codex_app_server_protocol_types::ThreadItem;
use codex_protocol::protocol::AgentStatus;
use std::collections::HashMap;

impl ThreadHistoryBuilder {
    pub(super) fn handle_collab_agent_spawn_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentSpawnBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::SpawnAgent,
                CollabAgentToolCallStatus::InProgress,
            ));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SpawnAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: Vec::new(),
            prompt: Some(payload.prompt.clone()),
            model: payload.model.clone(),
            reasoning_effort: payload.reasoning_effort.clone(),
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_collab_agent_spawn_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentSpawnEndEvent,
    ) {
        let has_receiver = payload.new_thread_id.is_some();
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ if has_receiver => CollabAgentToolCallStatus::Completed,
            _ => CollabAgentToolCallStatus::Failed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::SpawnAgent,
                status,
            ));
            return;
        }
        let (receiver_thread_ids, agents_states) = match &payload.new_thread_id {
            Some(id) => {
                let receiver_id = id.to_string();
                let received_status = CollabAgentState::from(payload.status.clone());
                (
                    vec![receiver_id.clone()],
                    [(receiver_id, received_status)].into_iter().collect(),
                )
            }
            None => (Vec::new(), HashMap::new()),
        };
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SpawnAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids,
            prompt: Some(payload.prompt.clone()),
            model: payload.model.clone(),
            reasoning_effort: payload.reasoning_effort.clone(),
            agents_states,
        });
    }

    pub(super) fn handle_collab_agent_interaction_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentInteractionBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::SendInput,
                CollabAgentToolCallStatus::InProgress,
            ));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SendInput,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: Some(payload.prompt.clone()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_collab_agent_interaction_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentInteractionEndEvent,
    ) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::SendInput,
                status,
            ));
            return;
        }
        let receiver_id = payload.receiver_thread_id.to_string();
        let received_status = CollabAgentState::from(payload.status.clone());
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SendInput,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id.clone()],
            prompt: Some(payload.prompt.clone()),
            model: None,
            reasoning_effort: None,
            agents_states: [(receiver_id, received_status)].into_iter().collect(),
        });
    }

    pub(super) fn handle_sub_agent_activity(
        &mut self,
        payload: &codex_protocol::protocol::SubAgentActivityEvent,
    ) {
        self.upsert_item_in_current_turn(ThreadItem::SubAgentActivity {
            id: payload.event_id.clone(),
            kind: payload.kind.into(),
            agent_thread_id: payload.agent_thread_id.to_string(),
            agent_path: String::from(payload.agent_path.clone()),
        });
    }

    pub(super) fn handle_collab_waiting_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabWaitingBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::Wait,
                CollabAgentToolCallStatus::InProgress,
            ));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: payload
                .receiver_thread_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_collab_waiting_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabWaitingEndEvent,
    ) {
        // Modern item events carry an explicit terminal outcome that this legacy
        // event cannot represent (for example, a failed wait with no receivers).
        // A terminal snapshot also covers waits reconciled by TurnAborted.
        let wait_status = |item: &ThreadItem| match item {
            ThreadItem::CollabAgentToolCall {
                id,
                tool: CollabAgentTool::Wait,
                status,
                ..
            } if id == &payload.call_id => Some(status.clone()),
            _ => None,
        };
        let current_owner = self.current_turn.as_ref().filter(|turn| self.retained_turn_override.as_ref().is_none_or(|id| id == &turn.id)).and_then(|turn| {
            turn.item_indexes.get(&payload.call_id).and_then(|&index| {
                wait_status(&turn.items[index]).map(|status| (turn.id.clone(), status))
            })
        });
        let owner = {
            let mut owners = current_owner.into_iter().chain(self.turns.iter().filter(|turn| self.retained_turn_override.as_ref().is_none_or(|id| id == &turn.id)).filter_map(|turn| {
                turn.items
                    .iter()
                    .find_map(&wait_status)
                    .map(|status| (turn.id.clone(), status))
            }));
            let first = owners.next();
            // An unscoped legacy event cannot disambiguate reused historical IDs.
            if owners.next().is_some() {
                return;
            }
            first
        };
        if owner.is_none() && self.rolled_back_wait_ids.contains(&payload.call_id) {
            return;
        }
        if owner
            .as_ref()
            .is_some_and(|(_, status)| *status != CollabAgentToolCallStatus::InProgress)
        {
            return;
        }
        let status = if payload
            .statuses
            .values()
            .any(|status| matches!(status, AgentStatus::Errored(_) | AgentStatus::NotFound))
        {
            CollabAgentToolCallStatus::Failed
        } else {
            CollabAgentToolCallStatus::Completed
        };
        if self.omit_tool_payloads {
            let item = Self::omitted_collab_item(&payload.call_id, CollabAgentTool::Wait, status);
            if let Some((turn_id, _)) = owner {
                self.upsert_item_in_turn_id(&turn_id, item);
            } else {
                self.upsert_item_in_current_turn(item);
            }
            return;
        }
        let mut receiver_thread_ids: Vec<String> =
            payload.statuses.keys().map(ToString::to_string).collect();
        receiver_thread_ids.sort();
        let agents_states = payload
            .statuses
            .iter()
            .map(|(id, status)| (id.to_string(), CollabAgentState::from(status.clone())))
            .collect();
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::Wait,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids,
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        };
        if let Some((turn_id, _)) = owner {
            self.upsert_item_in_turn_id(&turn_id, item);
        } else {
            self.upsert_item_in_current_turn(item);
        }
    }

    pub(super) fn handle_collab_close_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabCloseBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::CloseAgent,
                CollabAgentToolCallStatus::InProgress,
            ));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::CloseAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_collab_close_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabCloseEndEvent,
    ) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::CloseAgent,
                status,
            ));
            return;
        }
        let receiver_id = payload.receiver_thread_id.to_string();
        let agents_states = [(
            receiver_id.clone(),
            CollabAgentState::from(payload.status.clone()),
        )]
        .into_iter()
        .collect();
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::CloseAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        });
    }

    pub(super) fn handle_collab_resume_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabResumeBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::ResumeAgent,
                CollabAgentToolCallStatus::InProgress,
            ));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::ResumeAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    pub(super) fn handle_collab_resume_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabResumeEndEvent,
    ) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(
                &payload.call_id,
                CollabAgentTool::ResumeAgent,
                status,
            ));
            return;
        }
        let receiver_id = payload.receiver_thread_id.to_string();
        let agents_states = [(
            receiver_id.clone(),
            CollabAgentState::from(payload.status.clone()),
        )]
        .into_iter()
        .collect();
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::ResumeAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        });
    }

    fn omitted_collab_item(
        id: &str,
        tool: CollabAgentTool,
        status: CollabAgentToolCallStatus,
    ) -> ThreadItem {
        ThreadItem::CollabAgentToolCall {
            id: id.to_string(),
            tool,
            status,
            sender_thread_id: String::new(),
            receiver_thread_ids: Vec::new(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        }
    }
}
