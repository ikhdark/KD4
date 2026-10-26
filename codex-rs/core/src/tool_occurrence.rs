//! Order-aware pairing for legacy histories. New provider ID collisions are
//! assigned a fresh durable call ID before dispatch, not rejected or replayed.
use codex_protocol::models::ResponseItem;
use std::collections::HashMap;

pub(crate) fn identity(item: &ResponseItem) -> Option<(u8, bool, &str)> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => Some((0, true, call_id)),
        ResponseItem::FunctionCallOutput { call_id, .. } => Some((0, false, call_id)),
        ResponseItem::CustomToolCall { call_id, .. } => Some((1, true, call_id)),
        ResponseItem::CustomToolCallOutput { call_id, .. } => Some((1, false, call_id)),
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => Some((2, true, call_id)),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some((2, false, call_id)),
        _ => None,
    }
}

pub(crate) fn set_call_id(item: &mut ResponseItem, value: String) {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. }
        | ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => *call_id = value,
        _ => {}
    }
}

/// An output closes only a unique preceding unmatched call of the same kind.
/// A duplicate outstanding legacy ID is ambiguous, never evidence of success.
pub(crate) fn pairs(items: &[ResponseItem]) -> Vec<Option<usize>> {
    let mut pending = HashMap::new();
    let mut pairs = vec![None; items.len()];
    for (index, item) in items.iter().enumerate() {
        let Some((kind, call, id)) = identity(item) else {
            continue;
        };
        if call {
            pending
                .entry((kind, id))
                .and_modify(|old| *old = None)
                .or_insert(Some(index));
        } else if let Some(Some(source)) = pending.remove(&(kind, id)) {
            pairs[source] = Some(index);
            pairs[index] = Some(source);
        }
    }
    pairs
}

pub(crate) fn reused_call_ids(items: &[ResponseItem]) -> std::collections::HashSet<&str> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter_map(identity)
        .filter_map(|(_, call, id)| (call && !seen.insert(id)).then_some(id))
        .collect()
}
