//! Display-only Remote previews. Never mutate the stored or Desktop history.

use crate::outgoing_message::OutgoingMessage;
use codex_app_server_protocol_types::ServerNotification;
use codex_app_server_protocol_types::ThreadItem;
use serde_json::Value;

const TEXT_BYTES: usize = 8 * 1024;
const RESPONSE_TEXT_BYTES: usize = 4 * 1024 * 1024;
const NOTICE_TEXT_BYTES: usize = 64 * 1024;

fn text(value: &mut String, budget: &mut usize) {
    let limit = TEXT_BYTES.min(*budget);
    if value.len() <= limit {
        *budget -= value.len();
        return;
    }
    // Reserve room for the label, including at zero remaining content budget.
    let mut end = limit.saturating_sub(128);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = value.len() - end;
    value.truncate(end);
    value.push_str(&format!(
        "\n[Phone preview: {omitted} bytes omitted; full output remains in Desktop and saved history.]"
    ));
    *budget = budget.saturating_sub(value.len());
}

fn field(value: &mut Value, name: &str, budget: &mut usize) {
    if let Some(Value::String(value)) = value.get_mut(name) {
        text(value, budget);
    }
}

fn content(value: &mut Value, budget: &mut usize) {
    if let Some(parts) = value.as_array_mut() {
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text" | "inputText") => field(part, "text", budget),
                Some("resource") => {
                    if let Some(resource) = part.get_mut("resource") {
                        field(resource, "text", budget);
                    }
                }
                // Keep image data, resource identities and other typed blocks valid.
                _ => {}
            }
        }
    }
}

fn item(value: &mut Value, budget: &mut usize) {
    match value.get("type").and_then(Value::as_str) {
        Some("commandExecution") => {
            for name in ["aggregatedOutput", "stdout", "stderr"] {
                field(value, name, budget);
            }
        }
        Some("fileChange") => {
            if let Some(changes) = value.get_mut("changes").and_then(Value::as_array_mut) {
                for change in changes {
                    field(change, "diff", budget);
                }
            }
        }
        Some("dynamicToolCall") => {
            if let Some(parts) = value.get_mut("contentItems") {
                content(parts, budget);
            }
        }
        Some("mcpToolCall") => {
            if let Some(parts) = value.pointer_mut("/result/content") {
                content(parts, budget);
            }
        }
        // User/assistant text, identities, arguments, statuses and errors are exact.
        _ => {}
    }
}

fn page(value: &mut Value, budget: &mut usize) {
    if let Some(rows) = value.as_array_mut() {
        for row in rows {
            if let Some(items) = row.get_mut("items") {
                page(items, budget);
            } else {
                item(row, budget);
            }
        }
    }
}

pub(super) fn response(result: &mut Value) {
    let mut budget = RESPONSE_TEXT_BYTES;
    // Only public history containers, not arbitrary RPC result objects.
    for path in [
        "/thread/turns",
        "/turn/items",
        "/initialTurnsPage/data",
        "/data",
    ] {
        if let Some(rows) = result.pointer_mut(path) {
            page(rows, &mut budget);
        }
    }
}

fn typed_item(value: &mut ThreadItem, budget: &mut usize) {
    if !matches!(
        value,
        ThreadItem::CommandExecution { .. }
            | ThreadItem::FileChange { .. }
            | ThreadItem::DynamicToolCall { .. }
            | ThreadItem::McpToolCall { .. }
    ) {
        return;
    }
    // Work on a wire-shaped copy so the same allowlist serves history and live
    // items. Publish only after the complete typed conversion succeeds.
    let prepared = serde_json::to_value(&*value).and_then(|mut wire| {
        item(&mut wire, budget);
        serde_json::from_value(wire)
    });
    match prepared {
        Ok(prepared) => *value = prepared,
        Err(error) => tracing::warn!(%error, "could not prepare Remote tool preview"),
    }
}

pub(super) fn outgoing(message: &mut OutgoingMessage) {
    let OutgoingMessage::AppServerNotification(notification) = message else {
        return;
    };
    let mut budget = NOTICE_TEXT_BYTES;
    match notification {
        ServerNotification::ItemStarted(notice) => typed_item(&mut notice.item, &mut budget),
        ServerNotification::ItemCompleted(notice) => typed_item(&mut notice.item, &mut budget),
        ServerNotification::TurnDiffUpdated(notice) => text(&mut notice.diff, &mut budget),
        ServerNotification::FileChangePatchUpdated(notice) => {
            for change in &mut notice.changes {
                text(&mut change.diff, &mut budget);
            }
        }
        ServerNotification::CommandExecutionOutputDelta(notice) => {
            text(&mut notice.delta, &mut budget)
        }
        _ => {}
    }
}
