use super::*;
use crate::context_manager::ContextManager;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::items::AgentMessageContent;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some("msg-1".to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn multi_step_sampling_keeps_history_lazy_until_terminal_legacy_hook() {
    let modalities = codex_protocol::openai_models::default_input_modalities();
    let mut history = ContextManager::new();

    for step in 0..2 {
        let item = assistant_output_text(&format!("continuation-{step}"));
        history.record_items([&item], TruncationPolicy::Tokens(10_000));
        let snapshot = history.prompt_snapshot(&modalities);
        assert!(terminal_legacy_hook_history(true, snapshot).is_none());
    }

    let terminal_item = assistant_output_text("terminal");
    history.record_items([&terminal_item], TruncationPolicy::Tokens(10_000));
    let terminal = terminal_legacy_hook_history(false, history.prompt_snapshot(&modalities))
        .expect("terminal step retains the immutable history snapshot");
    assert_eq!(terminal.materialize().len(), 3);
}

#[test]
fn planning_schema_serialization_failure_adds_no_bytes_or_delimiter() {
    let mut failed = Sha256::new();
    failed.update(b"prefix");
    let expected_failed = failed.clone().finalize();
    let error = serde_json::Error::io(std::io::Error::other(
        "injected planning serialization failure",
    ));

    assert!(!append_planning_schema_digest(&mut failed, Err(error)));
    assert_eq!(failed.finalize(), expected_failed);

    let mut successful = Sha256::new();
    successful.update(b"prefix");
    assert!(append_planning_schema_digest(
        &mut successful,
        Ok(b"serialized tools")
    ));
    let mut expected_successful = Sha256::new();
    expected_successful.update(b"prefix");
    expected_successful.update(b"serialized tools");
    expected_successful.update([0xff]);
    assert_eq!(successful.finalize(), expected_successful.finalize());
}

#[tokio::test]
async fn trace_disabled_full_history_estimate_performs_zero_scans() {
    let items = [1_i64, 2, 3, 4];
    let scans = AtomicUsize::new(0);

    let disabled = maybe_estimate_history_for_trace(false, || async {
        Some(
            items
                .iter()
                .map(|item| {
                    scans.fetch_add(1, Ordering::SeqCst);
                    item
                })
                .sum(),
        )
    })
    .await;
    assert_eq!(disabled, None);
    assert_eq!(scans.load(Ordering::SeqCst), 0);

    let enabled = maybe_estimate_history_for_trace(true, || async {
        Some(
            items
                .iter()
                .map(|item| {
                    scans.fetch_add(1, Ordering::SeqCst);
                    item
                })
                .sum(),
        )
    })
    .await;
    assert_eq!(enabled, Some(10));
    assert_eq!(scans.load(Ordering::SeqCst), items.len());
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}
