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
        | RolloutItem::AdditionalContextState { .. }
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
    persisted_rollout_items_with_activity(items, history_mode, false)
}

/// Opt-in diagnostic events. These carry observations, not completed history items or
/// executable approval requests. Keeping them must not duplicate canonical completions.
pub fn is_rollout_activity_event(event: &EventMsg) -> bool {
    matches!(
        event,
        EventMsg::TurnPhaseChanged(_)
            | EventMsg::SafetyBuffering(_)
            | EventMsg::AgentReasoningSectionBreak(_)
            | EventMsg::ReasoningContentDelta(_)
            | EventMsg::ReasoningRawContentDelta(_)
            | EventMsg::PlanDelta(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::PatchApplyUpdated(_)
            | EventMsg::TurnDiff(_)
            | EventMsg::McpToolCallProgress(_)
            | EventMsg::ExecCommandOutputDelta(_)
            | EventMsg::TerminalInteraction(_)
    )
}

pub fn is_persisted_rollout_item_with_activity(
    item: &RolloutItem,
    history_mode: ThreadHistoryMode,
    capture_activity: bool,
) -> bool {
    is_persisted_rollout_item(item, history_mode)
        || (capture_activity
            && matches!(item, RolloutItem::EventMsg(event) if is_rollout_activity_event(event)))
}

pub fn persisted_rollout_items_with_activity(
    items: &[RolloutItem],
    history_mode: ThreadHistoryMode,
    capture_activity: bool,
) -> Vec<RolloutItem> {
    let mut persisted = Vec::new();
    for item in items {
        if is_persisted_rollout_item_with_activity(item, history_mode, capture_activity) {
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

/// Whether an `EventMsg` should be persisted in rollout files.
#[inline]
pub fn should_persist_event_msg(ev: &EventMsg, history_mode: ThreadHistoryMode) -> bool {
    match ev {
        EventMsg::ItemCompleted(event) => {
            // Paginated rollouts store TurnItems.
            // Legacy assistant events lack IDs; keep their canonical item for output recovery.
            // Legacy compaction markers also lack the canonical item's metadata.
            // Other legacy items are retained only when they lack a raw/legacy equivalent.
            // Extension compatibility events can omit canonical status and errors.
            matches!(history_mode, ThreadHistoryMode::Paginated)
                || matches!(&event.item, TurnItem::Extension(_))
                || matches!(event.item, TurnItem::Plan(_) | TurnItem::Sleep(_) | TurnItem::AgentMessage(_) | TurnItem::CommandExecution(_) | TurnItem::ContextCompaction(_) | TurnItem::UserMessage(_) | TurnItem::DynamicToolCall(_) | TurnItem::CollabAgentToolCall(_) | TurnItem::ImageView(_))
                || matches!(&event.item, TurnItem::DynamicToolCall(item)
                    if item.namespace.as_deref() == Some("codex.internal") && item.tool == "code_mode_cell")
        }
        EventMsg::ItemStarted(event) => matches!(&event.item, TurnItem::CommandExecution(item)
            if item.process_id.is_some()) || matches!(&event.item,
                TurnItem::DynamicToolCall(_) | TurnItem::CollabAgentToolCall(_) | TurnItem::ImageView(_)),
        EventMsg::TokenCount(_)
        | EventMsg::PlanUpdate(_)
        | EventMsg::ThreadGoalUpdated(_)
        | EventMsg::ThreadRolledBack(_)
        | EventMsg::TurnAborted(_)
        | EventMsg::TurnStarted(_)
        | EventMsg::TurnComplete(_)
        | EventMsg::ImportedTurnClosed(_)
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
        EventMsg::TurnPhaseChanged(_)
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

