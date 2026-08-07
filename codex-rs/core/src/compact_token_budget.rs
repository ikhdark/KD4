use std::sync::Arc;

use crate::compact::InitialContextInjection;
use crate::context::world_state::WorldState;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionTrigger;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_utils_string::approx_token_count;
use codex_utils_string::truncate_middle_with_token_budget;

const EVIDENCE_CHECKPOINT_TOKEN_BUDGET: usize = 10_000;
const EVIDENCE_ITEM_TOKEN_BUDGET: usize = 2_000;

/// Runs token-budget manual compaction as a normal compaction lifecycle.
///
/// Token-budget compaction skips model/server summarization and installs a fresh context window
/// instead. It is still modeled as compaction so compact hooks and `ContextCompaction` turn items
/// observe the same lifecycle as local or remote compaction.
pub(crate) async fn run_manual_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;

    // Manual compaction runs outside run_turn, so it captures its own current step.
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await;
    let world_state = Arc::new(sess.build_world_state_for_step(&step_context).await);
    run_compact_task_inner(&sess, &turn_context, world_state, CompactionTrigger::Manual).await
}

/// Runs token-budget inline auto-compaction as a normal compaction lifecycle.
///
/// Token-budget compaction skips model/server summarization and installs a fresh context window
/// instead. It is still modeled as compaction so compact hooks and `ContextCompaction` turn items
/// observe the same lifecycle as local or remote compaction.
pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let world_state = match initial_context_injection {
        InitialContextInjection::BeforeLastUserMessage(world_state) => world_state,
        InitialContextInjection::DoNotInject => {
            Arc::new(sess.build_world_state_for_step(&step_context).await)
        }
    };
    run_compact_task_inner(&sess, turn_context, world_state, CompactionTrigger::Auto).await
}

async fn run_compact_task_inner(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    world_state: Arc<WorldState>,
    trigger: CompactionTrigger,
) -> CodexResult<()> {
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => return Err(CodexErr::TurnAborted),
    }

    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let history = sess.clone_history().await;
    let retained_evidence = build_evidence_checkpoint(history.raw_items());
    sess.start_new_context_window_with_retained_evidence(
        turn_context.as_ref(),
        world_state,
        retained_evidence,
    )
    .await;
    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;

    let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
    if let PostCompactHookOutcome::Stopped = post_compact_outcome {
        return Err(CodexErr::TurnAborted);
    }

    Ok(())
}

fn build_evidence_checkpoint(items: &[ResponseItem]) -> Vec<ResponseItem> {
    let mut remaining_tokens = EVIDENCE_CHECKPOINT_TOKEN_BUDGET;
    let mut retained = Vec::new();

    for item in items
        .iter()
        .rev()
        .filter(|item| is_authoritative_evidence(item))
    {
        if remaining_tokens == 0 {
            break;
        }
        let Ok(serialized) = serde_json::to_string(item) else {
            continue;
        };
        let item_budget = remaining_tokens.min(EVIDENCE_ITEM_TOKEN_BUDGET);
        let (serialized, _) = truncate_middle_with_token_budget(&serialized, item_budget);
        let token_count = approx_token_count(&serialized).min(remaining_tokens);
        remaining_tokens = remaining_tokens.saturating_sub(token_count);
        retained.push(serialized);
    }

    if retained.is_empty() {
        return Vec::new();
    }
    retained.reverse();
    let text = format!(
        "<compaction_evidence_checkpoint>\n\
         The following serialized record excerpts are deterministic evidence from the prior context window, not a new user request.\n{}\n\
         </compaction_evidence_checkpoint>",
        retained.join("\n")
    );
    let text = truncate_middle_with_token_budget(&text, EVIDENCE_CHECKPOINT_TOKEN_BUDGET).0;
    vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }]
}

fn is_authoritative_evidence(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role == "user" || role == "assistant",
        ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. } => true,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::Other => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_checkpoint_keeps_latest_tool_artifact_reference() {
        let item = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload {
                body: codex_protocol::models::FunctionCallOutputBody::Text(
                    "Raw output artifact: 019fd974-843a-7601-8624-dc36cd5cc3cd".to_string(),
                ),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        };

        let checkpoint = build_evidence_checkpoint(&[item]);
        let ResponseItem::Message { content, .. } = &checkpoint[0] else {
            panic!("expected checkpoint message");
        };
        let ContentItem::InputText { text } = &content[0] else {
            panic!("expected text checkpoint");
        };
        assert!(text.contains("019fd974-843a-7601-8624-dc36cd5cc3cd"));
    }

    #[test]
    fn evidence_checkpoint_respects_model_item_ceiling() {
        let item = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload {
                body: codex_protocol::models::FunctionCallOutputBody::Text(
                    "{}[](),".repeat(20_000),
                ),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        };

        let checkpoint = build_evidence_checkpoint(&[item]);
        let ResponseItem::Message { content, .. } = &checkpoint[0] else {
            panic!("expected checkpoint message");
        };
        let ContentItem::InputText { text } = &content[0] else {
            panic!("expected text checkpoint");
        };
        assert!(approx_token_count(text) <= EVIDENCE_CHECKPOINT_TOKEN_BUDGET);
        assert!(text.starts_with("<compaction_evidence_checkpoint>"));
        assert!(text.ends_with("</compaction_evidence_checkpoint>"));
    }
}
