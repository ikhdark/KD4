//! Implements the MultiAgentV2 collaboration tool surface.

use crate::FunctionCallError;
use crate::agent::AgentStatus;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::*;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::AgentPath;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_tools::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

pub(crate) use interrupt_agent::Handler as InterruptAgentHandler;
pub(crate) use list_agents::Handler as ListAgentsHandler;
pub(crate) use message_tool::FollowupTaskHandler;
pub(crate) use message_tool::SendMessageHandler;
pub(crate) use spawn::Handler as SpawnAgentHandler;
pub(crate) use task::AbandonAgentTaskHandler;
pub(crate) use task::AmendAgentTaskHandler;
pub(crate) use task::GetAgentTaskHandler;
pub(crate) use task::SetAgentGateHandler;
pub(crate) use task::SubmitAgentReceiptHandler;
pub(crate) use task::WaiveAgentGateHandler;
pub(crate) use wait::Handler as WaitAgentHandler;

mod interrupt_agent;
mod list_agents;
mod message_tool;
mod spawn;
mod task;
pub(crate) mod wait;

fn task_store_error_detail(
    tool_name: &'static str,
    error: codex_agent_task_store::StoreError,
) -> String {
    use codex_agent_task_store::StoreError;

    match error {
        error @ (StoreError::Io(_)
        | StoreError::Sql(_)
        | StoreError::Migration(_)
        | StoreError::Json(_)
        | StoreError::CorruptData(_)) => {
            // Keep storage details out of tool output, but retain the cause for diagnosis.
            tracing::error!(tool_name, error = %error, "typed task store operation failed");
            "the typed task store is unavailable or contains invalid persisted state".to_string()
        }
        error => error.to_string(),
    }
}

pub(crate) async fn emit_sub_agent_activity(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    item: SubAgentActivityItem,
) {
    session
        .emit_turn_item_completed(turn, TurnItem::SubAgentActivity(item))
        .await;
}

pub(super) fn communication_from_tool_message(
    author: AgentPath,
    recipient: AgentPath,
    message: String,
) -> InterAgentCommunication {
    InterAgentCommunication::new_encrypted(
        author,
        recipient,
        Vec::new(),
        message,
        /*trigger_turn*/ true,
    )
}

pub(super) fn communication_from_plaintext_message(
    author: AgentPath,
    recipient: AgentPath,
    message: String,
) -> InterAgentCommunication {
    InterAgentCommunication::new(
        author,
        recipient,
        Vec::new(),
        message,
        /*trigger_turn*/ true,
    )
}

#[cfg(test)]
mod store_error_tests {
    use super::*;
    use codex_agent_task_store::StoreError;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;
    use tracing_test::internal::MockWriter;

    pub(super) fn assert_error_reporting(
        tool_name: &'static str,
        convert: impl Fn(StoreError) -> FunctionCallError,
    ) {
        let buffer: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(MockWriter::new(buffer))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let errors = [
            StoreError::Io(std::io::Error::other("task store disk failure")),
            StoreError::Sql(sqlx::Error::PoolClosed),
            StoreError::Migration(sqlx::migrate::MigrateError::VersionMissing(7)),
            StoreError::Json(serde_json::from_str::<JsonValue>("{").unwrap_err()),
            StoreError::CorruptData("invalid stored assignment".to_string()),
        ];
        for error in errors {
            buffer.lock().unwrap().clear();
            let cause = error.to_string();
            let FunctionCallError::RespondToModel(detail) = convert(error) else {
                panic!("store failure must remain a tool response");
            };
            assert_eq!(
                detail,
                format!(
                    "{tool_name}: the typed task store is unavailable or contains invalid persisted state"
                )
            );
            let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
            assert_eq!(logs.matches("typed task store operation failed").count(), 1);
            assert!(logs.contains("ERROR"), "{logs}");
            assert!(
                logs.contains(&format!("tool_name=\"{tool_name}\"")),
                "{logs}"
            );
            assert!(logs.contains(&cause), "{logs}");
        }

        buffer.lock().unwrap().clear();
        let error = StoreError::InvalidScope("outside assigned scope".to_string());
        let expected = format!("{tool_name}: {error}");
        let FunctionCallError::RespondToModel(detail) = convert(error) else {
            panic!("validation rejection must remain a tool response");
        };
        assert_eq!(detail, expected);
        assert!(buffer.lock().unwrap().is_empty());
    }
}
