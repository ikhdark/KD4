//! Passive, owner-produced nested operation receipts for capture/export.

use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_tools::ToolName;
use serde_json::Value as JsonValue;

use super::ExecContext;
use super::ToolPayload;

pub(super) fn publish(
    exec: &ExecContext,
    cell_id: &CellId,
    call_id: &str,
    parent_call_id: Option<&str>,
    runtime_tool_call_id: &str,
    tool_name: &ToolName,
    payload: &ToolPayload,
    result: &JsonValue,
    failed: bool,
    duration: Duration,
) -> Option<futures::future::Shared<futures::future::BoxFuture<'static, ()>>> {
    // Keep the executed payload, including hook replacements, and the native
    // result before script-visible compaction. Neither JS prints nor cell/turn
    // completion can substitute for this receipt.
    let payload = match payload {
        ToolPayload::Function { arguments } => serde_json::json!({
            "type": "function", "arguments": arguments,
        }),
        ToolPayload::ToolSearch { arguments } => serde_json::json!({
            "type": "tool_search", "arguments": arguments,
        }),
        ToolPayload::Custom { input } => serde_json::json!({
            "type": "custom", "input": input,
        }),
    };
    let evidence = serde_json::json!({"payload": payload, "result": result});
    let arguments = serde_json::json!({
        "call_id": call_id, "parent_call_id": parent_call_id,
        "parent_cell_id": cell_id.as_str(),
        "runtime_tool_call_id": runtime_tool_call_id,
        "tool_name": tool_name.to_string(),
    });
    let session = Arc::clone(&exec.session);
    let turn = Arc::clone(&exec.turn);
    let call_id = call_id.to_string();
    // Use the existing bounded, session-owned retention queue. Cancellation of
    // the cell observer cannot discard an accepted receipt, and normal cell
    // completion observes this tail before retiring its packet.
    exec.session.services.code_mode_service.queue_packet_retention(
        &exec.session,
        cell_id,
        async move {
            let text = evidence.to_string();
            let text = if text.len() <= 64 * 1024 {
                text
            } else {
                let canonical = codex_tools::CanonicalToolResult::json(evidence);
                // A producer's stdout artifact is not the native JSON receipt.
                // Retain this exact envelope with the existing artifact owner.
                let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact_with_id(
                    &turn.config.codex_home,
                    &session.thread_id.to_string(),
                    &canonical,
                    crate::tools::command_output_artifact::ToolOutputArtifactId::new(),
                ).await;
                let content = if artifact.complete && let Some(id) = artifact.artifact_id() {
                    session.register_tool_artifact_origin(
                        id.clone(), call_id.clone(), canonical.exact_bytes, canonical.sha256.clone(),
                    ).await;
                    serde_json::json!({"evidence_reference": {
                        "artifact_id": id, "canonical_sha256": canonical.sha256,
                        "canonical_bytes": canonical.exact_bytes,
                        "recovery": {"tool": "read_tool_output", "arguments": {
                            "artifact_id": id, "selectors": [{"kind": "bytes", "start": 0, "end": canonical.exact_bytes}],
                        }},
                    }})
                } else {
                    // Retention failure must not manufacture a successful
                    // receipt or leave an unresolvable reference in the export.
                    serde_json::json!({"evidence_unavailable": true,
                        "canonical_sha256": canonical.sha256,
                        "canonical_bytes": canonical.exact_bytes,
                        "notice": "Native operation receipt retention failed; do not replay effects."})
                };
                content.to_string()
            };
            // Materialized items persist in both legacy and paginated rollouts;
            // a bare DynamicToolCallResponse is transient and would lose this
            // receipt on history export/resume. No actionable request is sent.
            session.emit_turn_item_completed(&turn, codex_protocol::items::TurnItem::DynamicToolCall(
                codex_protocol::items::DynamicToolCallItem {
                    // Commands already publish items under the nested call ID.
                    // Keep this independent receipt from replacing that item.
                    id: format!("code-mode-tool-call:{call_id}"),
                    namespace: Some("codex.internal".to_string()),
                    tool: "code_mode_tool_call".to_string(),
                    arguments,
                    content_items: Some(vec![codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputText {
                        text,
                    }]),
                    status: if failed {
                        codex_protocol::items::DynamicToolCallStatus::Failed
                    } else {
                        codex_protocol::items::DynamicToolCallStatus::Completed
                    },
                    success: Some(!failed),
                    error: None,
                    duration: Some(duration),
                },
            )).await;
        },
    )
}
