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

/// The kind of a host-side storage failure, or `None` when the store rejected
/// the call itself (scope, state, or argument validation).
fn task_store_failure_kind(error: &codex_agent_task_store::StoreError) -> Option<&'static str> {
    use codex_agent_task_store::StoreError;

    match error {
        StoreError::Io(_) => Some("storage I/O"),
        StoreError::Sql(_) => Some("database"),
        StoreError::Migration(_) => Some("schema migration"),
        StoreError::Json(_) => Some("persisted record decoding"),
        StoreError::CorruptData(_) => Some("persisted state consistency"),
        _ => None,
    }
}

fn task_store_error_detail(
    tool_name: &'static str,
    error: codex_agent_task_store::StoreError,
) -> String {
    let Some(kind) = task_store_failure_kind(&error) else {
        return error.to_string();
    };
    // Keep storage details out of tool output, but retain the cause for diagnosis.
    let diagnostic_id = uuid::Uuid::now_v7().to_string();
    tracing::error!(tool_name, diagnostic_id, error = %error, "typed task store operation failed");
    let reason_code = match &error {
        codex_agent_task_store::StoreError::Migration(sqlx::migrate::MigrateError::VersionMissing(_) | sqlx::migrate::MigrateError::VersionMismatch(_)) => "incompatible_schema",
        codex_agent_task_store::StoreError::Migration(_) => "schema_migration_failed",
        codex_agent_task_store::StoreError::Json(_) | codex_agent_task_store::StoreError::CorruptData(_) => "persisted_integrity",
        codex_agent_task_store::StoreError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => "storage_permission_denied",
        codex_agent_task_store::StoreError::Io(_) => "storage_io",
        codex_agent_task_store::StoreError::Sql(_) if error.is_retryable() => "storage_contention",
        codex_agent_task_store::StoreError::Sql(sqlx::Error::Database(database)) => {
            match database.code().and_then(|code| code.parse::<i32>().ok()).map(|code| code & 0xff) {
                Some(3 | 8 | 23) => "storage_permission_denied",
                Some(10) => "storage_io",
                Some(11 | 26) => "persisted_integrity",
                Some(19) => "storage_constraint_conflict",
                _ => "storage_database",
            }
        }
        _ => "storage_database",
    };
    let recovery = serde_json::json!({
        "reason_code": reason_code, "retryable": error.is_retryable(),
        "affected_scope": "requested_task_store_operation",
        "next_safe_action": if error.is_retryable() { "inspect_committed_assignment_then_bounded_retry" }
            else if reason_code == "incompatible_schema" { "use_compatible_runtime_preserve_database" }
            else { "retain_obligation_and_repair_affected_state_using_host_diagnostic" },
        "diagnostic_id": diagnostic_id,
    });
    if error.is_retryable() {
        return format!(
            "the typed task store is temporarily unavailable ({kind} failure; the cause is in the host log). This is not caused by the call's arguments. Recovery may succeed after availability changes; inspect retained assignment state before any retry, because a storage error does not prove that a mutation did not commit; recovery={recovery}"
        );
    }
    // Say plainly that this is not the caller's mistake and will not clear on
    // a retry; a bare "unavailable" sent models into retry and re-plan rounds.
    format!(
        "the typed task store is unavailable or contains invalid persisted state ({kind} failure; the cause is in the host log). This is not caused by the call's arguments, and repeating the call is not expected to succeed; recovery={recovery}"
    )
}

/// A whole-call store failure behind the handler's context. Storage causes get
/// the same redacted recovery classification as spawn and task tools.
fn task_store_context_error(
    tool_name: &'static str,
    context: &str,
    error: codex_agent_task_store::StoreError,
) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "{context}: {}",
        task_store_error_detail(tool_name, error)
    ))
}

fn assignment_local_failure(error: &codex_agent_task_store::StoreError) -> bool {
    matches!(error, codex_agent_task_store::StoreError::Io(_)
        | codex_agent_task_store::StoreError::Json(_)
        | codex_agent_task_store::StoreError::CorruptData(_)
        | codex_agent_task_store::StoreError::AssignmentNotFound(_)
        | codex_agent_task_store::StoreError::AttemptNotFound(_))
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

