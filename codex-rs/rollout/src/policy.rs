use crate::protocol::EventMsg;
use crate::protocol::RolloutItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;

/// Whether a rollout `item` should be persisted in rollout files.
pub fn is_persisted_rollout_item(item: &RolloutItem, history_mode: ThreadHistoryMode) -> bool {
    match item {
        RolloutItem::ResponseItem(item) => should_persist_response_item(item),
        RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. } => true,
        RolloutItem::EventMsg(ev) => should_persist_event_msg(ev, history_mode),
        // Persist Codex executive markers so we can analyze flows (e.g., compaction, API turns).
        RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::ToolManifest(_)
        | RolloutItem::SamplingBoundary(_)
        | RolloutItem::SessionMeta(_) => true,
    }
}

/// Return the rollout items that should be persisted for a live append.
pub fn persisted_rollout_items(
    items: &[RolloutItem],
    history_mode: ThreadHistoryMode,
) -> Vec<RolloutItem> {
    let mut persisted = Vec::new();
    for item in items {
        if is_persisted_rollout_item(item, history_mode) {
            persisted.push(item.clone());
        }
    }
    persisted
}

/// Whether a `ResponseItem` should be persisted in rollout files.
#[inline]
pub fn should_persist_response_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::Other => false,
    }
}

/// Whether a `ResponseItem` should be persisted for the memories.
#[inline]
pub fn should_persist_response_item_for_memories(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role != "developer",
        ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::WebSearchCall { .. } => true,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => false,
    }
}

/// Whether an `EventMsg` should be persisted in rollout files.
#[inline]
pub fn should_persist_event_msg(ev: &EventMsg, history_mode: ThreadHistoryMode) -> bool {
    match ev {
        EventMsg::ItemCompleted(event) => {
            // Paginated rollouts store TurnItems.
            // Legacy assistant events lack IDs; keep their canonical item for output recovery.
            // Other legacy items are retained only when they lack a raw/legacy equivalent.
            matches!(history_mode, ThreadHistoryMode::Paginated)
                || matches!(event.item, TurnItem::Plan(_) | TurnItem::Sleep(_) | TurnItem::AgentMessage(_))
        }
        EventMsg::TokenCount(_)
        | EventMsg::PlanUpdate(_)
        | EventMsg::ThreadGoalUpdated(_)
        | EventMsg::ThreadRolledBack(_)
        | EventMsg::TurnAborted(_)
        | EventMsg::TurnStarted(_)
        | EventMsg::TurnComplete(_)
        // These have no completed TurnItem equivalent. Preserve failures and
        // host interventions in both history modes for replay and diagnosis.
        | EventMsg::Error(_)
        | EventMsg::Warning(_)
        | EventMsg::StreamError(_)
        | EventMsg::ModelReroute(_)
        | EventMsg::ModelVerification(_)
        | EventMsg::HookStarted(_)
        | EventMsg::HookCompleted(_)
        | EventMsg::ThreadSettingsApplied(_) => true,

        // Only persist these legacy events when the thread's history mode is Legacy.
        // New, paginated rollouts persist ItemCompleted events with TurnItems.
        EventMsg::UserMessage(_)
        | EventMsg::AgentMessage(_)
        | EventMsg::AgentReasoning(_)
        | EventMsg::AgentReasoningRawContent(_)
        | EventMsg::EnteredReviewMode(_)
        | EventMsg::ExitedReviewMode(_)
        | EventMsg::PatchApplyEnd(_)
        | EventMsg::ContextCompacted(_)
        | EventMsg::McpToolCallEnd(_)
        | EventMsg::WebSearchEnd(_)
        | EventMsg::ImageGenerationEnd(_)
        | EventMsg::SubAgentActivity(_) => matches!(history_mode, ThreadHistoryMode::Legacy),

        // Transient, non-durable events.
        | EventMsg::ExecCommandEnd(_)
        | EventMsg::ViewImageToolCall(_)
        | EventMsg::CollabAgentSpawnEnd(_)
        | EventMsg::CollabAgentInteractionEnd(_)
        | EventMsg::CollabWaitingEnd(_)
        | EventMsg::CollabCloseEnd(_)
        | EventMsg::CollabResumeEnd(_)
        | EventMsg::DynamicToolCallRequest(_)
        | EventMsg::DynamicToolCallResponse(_)
        | EventMsg::SafetyBuffering(_)
        | EventMsg::TurnModerationMetadata(_)
        | EventMsg::AgentReasoningSectionBreak(_)
        | EventMsg::RawResponseItem(_)
        | EventMsg::SessionConfigured(_)
        | EventMsg::McpToolCallBegin(_)
        | EventMsg::McpToolCallProgress(_)
        | EventMsg::ExecCommandBegin(_)
        | EventMsg::TerminalInteraction(_)
        | EventMsg::ExecCommandOutputDelta(_)
        | EventMsg::ExecApprovalRequest(_)
        | EventMsg::RequestPermissions(_)
        | EventMsg::RequestUserInput(_)
        | EventMsg::ElicitationRequest(_)
        | EventMsg::ApplyPatchApprovalRequest(_)
        | EventMsg::PatchApplyBegin(_)
        | EventMsg::PatchApplyUpdated(_)
        | EventMsg::TurnDiff(_)
        | EventMsg::McpStartupUpdate(_)
        | EventMsg::McpStartupComplete(_)
        | EventMsg::WebSearchBegin(_)
        | EventMsg::ShutdownComplete
        | EventMsg::DeprecationNotice(_)
        | EventMsg::ItemStarted(_)
        | EventMsg::AgentMessageContentDelta(_)
        | EventMsg::PlanDelta(_)
        | EventMsg::ReasoningContentDelta(_)
        | EventMsg::ReasoningRawContentDelta(_)
        | EventMsg::ImageGenerationBegin(_)
        | EventMsg::CollabAgentSpawnBegin(_)
        | EventMsg::CollabAgentInteractionBegin(_)
        | EventMsg::CollabWaitingBegin(_)
        | EventMsg::CollabCloseBegin(_)
        | EventMsg::CollabResumeBegin(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::persisted_rollout_items;
    use super::should_persist_event_msg;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::McpToolCallProgressEvent;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::ThreadHistoryMode;
    use serde_json::json;

    #[tokio::test]
    async fn live_append_preserves_failures_and_host_interventions_in_both_history_modes() {
        let source = tempfile::tempdir().unwrap();
        let hook_run = json!({
            "id": "hook-1",
            "event_name": "stop",
            "handler_type": "command",
            "execution_mode": "sync",
            "scope": "turn",
            "source_path": source.path().join("hooks.json"),
            "display_order": 0,
            "status": "running",
            "status_message": null,
            "started_at": 100,
            "completed_at": null,
            "duration_ms": null,
            "entries": []
        });
        let mut completed = hook_run.clone();
        completed["status"] = json!("blocked");
        completed["completed_at"] = json!(125);
        completed["duration_ms"] = json!(25);
        completed["entries"] = json!([{"kind": "stop", "text": "validation required"}]);
        let expected = vec![
            json!({"type": "error", "message": "request failed", "codex_error_info": null}),
            json!({"type": "warning", "message": "tools stopped after a repeated cycle"}),
            json!({"type": "stream_error", "message": "Reconnecting... 1/4", "codex_error_info": {"response_stream_disconnected": {"http_status_code": 503}}, "additional_details": "connection reset before response completed"}),
            json!({"type": "model_reroute", "from_model": "requested", "to_model": "served", "reason": "high_risk_cyber_activity"}),
            json!({"type": "model_verification", "verifications": ["trusted_access_for_cyber"]}),
            json!({"type": "hook_started", "turn_id": "turn-1", "run": hook_run}),
            json!({"type": "hook_completed", "turn_id": "turn-1", "run": completed}),
        ]
        .into_iter()
        .map(|value| RolloutItem::EventMsg(serde_json::from_value(value).unwrap()))
        .collect::<Vec<_>>();
        let mut live_items = expected.clone();
        live_items.insert(
            1,
            RolloutItem::EventMsg(EventMsg::McpToolCallProgress(McpToolCallProgressEvent {
                call_id: "call-1".to_string(),
                message: "halfway".to_string(),
            })),
        );

        for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            let persisted = persisted_rollout_items(&live_items, history_mode);
            // Compare the complete payloads: retaining only a count or final
            // hook status would lose the reason a turn was interrupted.
            assert_eq!(
                serde_json::to_value(&persisted).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
            let home = tempfile::tempdir().unwrap();
            let config = crate::config::RolloutConfig {
                codex_home: home.path().to_path_buf(),
                sqlite_home: home.path().to_path_buf(),
                cwd: home.path().to_path_buf(),
                model_provider_id: "test-provider".to_string(),
                generate_memories: false,
            };
            let thread_id = codex_protocol::ThreadId::new();
            let recorder = crate::recorder::RolloutRecorder::new(
                &config,
                crate::recorder::RolloutRecorderParams::new(
                    thread_id,
                    None,
                    None,
                    codex_protocol::protocol::SessionSource::Exec,
                    None,
                    "durable-events-test".to_string(),
                    codex_protocol::models::BaseInstructions::default(),
                    Vec::new(),
                )
                .with_history_mode(history_mode),
            )
            .await
            .unwrap();
            recorder.record_canonical_items(&persisted).await.unwrap();
            recorder.persist().await.unwrap();
            recorder.shutdown().await.unwrap();
            let (replayed, loaded_id, errors) =
                crate::recorder::RolloutRecorder::load_rollout_items(recorder.rollout_path())
                    .await
                    .unwrap();
            assert_eq!(loaded_id, Some(thread_id));
            assert_eq!(errors, 0);
            let events = replayed
                .into_iter()
                .filter(|item| matches!(item, RolloutItem::EventMsg(_)))
                .collect::<Vec<_>>();
            assert_eq!(
                serde_json::to_value(events).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
        }
    }

    #[test]
    fn committed_checklist_updates_are_durable_in_both_history_modes() {
        let event = EventMsg::PlanUpdate(codex_protocol::plan_tool::UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        });
        for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            assert!(should_persist_event_msg(&event, history_mode));
        }
    }

    #[test]
    fn mcp_tool_call_progress_is_transient() {
        let event = EventMsg::McpToolCallProgress(McpToolCallProgressEvent {
            call_id: "call-1".to_string(),
            message: "halfway".to_string(),
        });
        for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            assert!(!should_persist_event_msg(&event, history_mode));
        }
    }
}
