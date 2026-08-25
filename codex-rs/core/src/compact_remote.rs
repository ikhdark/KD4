use std::collections::HashSet;

use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::compact::insert_compaction_initial_context;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::ContextManager;
use crate::context_manager::estimate_item_token_count;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;

const CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE: &str =
    "Output exceeded the available model context and was truncated";
const REMOTE_COMPACTION_TOOL_RECEIPT_MAX_TOKENS: usize = 2_000;
const REMOTE_COMPACTION_TOOL_RECEIPT_MAX_ITEMS: usize = 32;

pub(crate) async fn process_compacted_history(
    sess: &Session,
    turn_context: &TurnContext,
    mut compacted_history: Vec<ResponseItem>,
    initial_context_injection: &InitialContextInjection,
) -> (
    Vec<ResponseItem>,
    Option<WorldStateSnapshot>,
    Vec<codex_protocol::protocol::ContextFragmentDigest>,
) {
    // Preserve the caller-selected replacement ordering. The default `AtStart` path retains the
    // cacheable prompt prefix while still leaving the summary or compaction item last.
    let (initial_context, world_state_baseline, fragment_digests) =
        build_compaction_initial_context(sess, turn_context, initial_context_injection).await;

    compacted_history = bounded_remote_compacted_history(compacted_history);
    (
        insert_compaction_initial_context(
            compacted_history,
            initial_context,
            initial_context_injection,
        ),
        world_state_baseline,
        fragment_digests,
    )
}

/// Returns whether an item from remote compaction output should be preserved.
///
/// Called while processing the model-provided compacted transcript, before we
/// append fresh canonical context from the current session.
///
/// Raw messages are consumed evidence after the remote endpoint has produced an opaque
/// compaction item. Only that item and recoverable tool receipts remain eligible; fresh current
/// instructions and durable task state are injected separately by the caller.
pub(crate) fn should_keep_compacted_history_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
    )
}

fn bounded_remote_compacted_history(items: Vec<ResponseItem>) -> Vec<ResponseItem> {
    let mut retained_indices = HashSet::new();
    let mut remaining_tokens = REMOTE_COMPACTION_TOOL_RECEIPT_MAX_TOKENS;
    let mut retained_tool_items = 0usize;
    let mut retained_compaction = false;

    for (index, item) in items.iter().enumerate().rev() {
        match item {
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                if !retained_compaction =>
            {
                retained_compaction = true;
                retained_indices.insert(index);
            }
            item if should_keep_compacted_history_item(item) => {
                let Some(group) = complete_tool_receipt_indices(&items, index) else {
                    continue;
                };
                if !group
                    .iter()
                    .any(|index| item_has_recoverable_artifact_reference(&items[*index]))
                {
                    continue;
                }
                if group.iter().any(|index| retained_indices.contains(index)) {
                    continue;
                }
                if retained_tool_items.saturating_add(group.len())
                    > REMOTE_COMPACTION_TOOL_RECEIPT_MAX_ITEMS
                {
                    continue;
                }
                let tokens = group.iter().fold(0usize, |total, index| {
                    let item_tokens =
                        usize::try_from(estimate_item_token_count(&items[*index]).max(1))
                            .unwrap_or(usize::MAX);
                    total.saturating_add(item_tokens)
                });
                if tokens <= remaining_tokens {
                    retained_tool_items = retained_tool_items.saturating_add(group.len());
                    remaining_tokens = remaining_tokens.saturating_sub(tokens);
                    retained_indices.extend(group);
                }
            }
            _ => {}
        }
    }

    items
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| retained_indices.contains(&index).then_some(item))
        .collect()
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ToolReceiptKind {
    Function,
    Custom,
    Search,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ToolReceiptSide {
    Call,
    Output,
}

fn complete_tool_receipt_indices(items: &[ResponseItem], index: usize) -> Option<Vec<usize>> {
    let (kind, side, call_id) = tool_receipt_identity(&items[index])?;
    let counterpart_side = match side {
        ToolReceiptSide::Call => ToolReceiptSide::Output,
        ToolReceiptSide::Output => ToolReceiptSide::Call,
    };
    let counterpart = items
        .iter()
        .enumerate()
        .find_map(|(candidate_index, item)| {
            let (candidate_kind, candidate_side, candidate_call_id) = tool_receipt_identity(item)?;
            (candidate_kind == kind
                && candidate_side == counterpart_side
                && candidate_call_id == call_id)
                .then_some(candidate_index)
        })?;
    let mut group = vec![index, counterpart];
    group.sort_unstable();
    group.dedup();
    Some(group)
}

fn item_has_recoverable_artifact_reference(item: &ResponseItem) -> bool {
    let text = match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.body.to_text(),
        _ => None,
    };
    text.is_some_and(|text| has_recoverable_artifact_reference(&text))
}

fn has_recoverable_artifact_reference(text: &str) -> bool {
    let lowercase = text.to_ascii_lowercase();
    let has_recovery_cue = lowercase.contains("read_tool_output")
        || lowercase.contains("raw output artifact:")
        || lowercase.contains("raw_output_artifact_id")
        || lowercase.contains("available as artifact")
        || lowercase.contains("retained as artifact");
    has_recovery_cue
        && text
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
            .any(|candidate| uuid::Uuid::parse_str(candidate).is_ok())
}

fn tool_receipt_identity(item: &ResponseItem) -> Option<(ToolReceiptKind, ToolReceiptSide, &str)> {
    match item {
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::FunctionCall { call_id, .. } => {
            Some((ToolReceiptKind::Function, ToolReceiptSide::Call, call_id))
        }
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            Some((ToolReceiptKind::Function, ToolReceiptSide::Output, call_id))
        }
        ResponseItem::CustomToolCall { call_id, .. } => {
            Some((ToolReceiptKind::Custom, ToolReceiptSide::Call, call_id))
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            Some((ToolReceiptKind::Custom, ToolReceiptSide::Output, call_id))
        }
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => Some((ToolReceiptKind::Search, ToolReceiptSide::Call, call_id)),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some((ToolReceiptKind::Search, ToolReceiptSide::Output, call_id)),
        _ => None,
    }
}

pub(crate) fn trim_function_call_history_to_fit_context_window(
    history: &mut ContextManager,
    turn_context: &TurnContext,
    base_instructions: &BaseInstructions,
) -> (usize, i64) {
    let Some(context_window) = turn_context.model_context_window() else {
        return (0, 0);
    };
    let item_token_counts = history
        .raw_items()
        .iter()
        .map(estimate_item_token_count)
        .collect::<Vec<_>>();
    let mut estimated_tokens =
        i128::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i128::MAX);
    for item_tokens in &item_token_counts {
        estimated_tokens = estimated_tokens.saturating_add(i128::from(*item_tokens));
    }
    let estimated_tokens_before = estimated_tokens;
    let context_window = i128::from(context_window);
    let mut replacements = Vec::new();

    for index in (0..history.raw_items().len()).rev() {
        if estimated_tokens <= context_window {
            break;
        }
        let Some(rewritten_item) = history
            .raw_items()
            .get(index)
            .and_then(rewritten_output_for_context_window)
        else {
            break;
        };
        let rewritten_tokens = estimate_item_token_count(&rewritten_item);
        estimated_tokens = estimated_tokens
            .saturating_sub(i128::from(item_token_counts[index]))
            .saturating_add(i128::from(rewritten_tokens));
        replacements.push((index, rewritten_item));
    }

    let rewritten_outputs = replacements.len();
    if rewritten_outputs > 0 {
        let mut items = history.raw_items().to_vec();
        for (index, rewritten_item) in replacements {
            items[index] = rewritten_item;
        }
        history.replace(items);
    }

    let estimated_deleted_tokens = estimated_tokens_before.saturating_sub(estimated_tokens);
    let estimated_deleted_tokens = i64::try_from(estimated_deleted_tokens).unwrap_or_else(|_| {
        if estimated_deleted_tokens.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    });
    (rewritten_outputs, estimated_deleted_tokens)
}

fn rewritten_output_for_context_window(item: &ResponseItem) -> Option<ResponseItem> {
    Some(match item {
        ResponseItem::FunctionCallOutput {
            id,
            call_id,
            output,
            internal_chat_message_metadata_passthrough: metadata,
        } => ResponseItem::FunctionCallOutput {
            id: id.clone(),
            call_id: call_id.clone(),
            output: truncated_output_payload(output),
            internal_chat_message_metadata_passthrough: metadata.clone(),
        },
        ResponseItem::CustomToolCallOutput {
            id,
            call_id,
            name,
            output,
            internal_chat_message_metadata_passthrough: metadata,
        } => ResponseItem::CustomToolCallOutput {
            id: id.clone(),
            call_id: call_id.clone(),
            name: name.clone(),
            output: truncated_output_payload(output),
            internal_chat_message_metadata_passthrough: metadata.clone(),
        },
        ResponseItem::ToolSearchOutput {
            id,
            call_id,
            status,
            execution,
            omitted_result_count,
            internal_chat_message_metadata_passthrough: metadata,
            ..
        } => ResponseItem::ToolSearchOutput {
            id: id.clone(),
            call_id: call_id.clone(),
            status: status.clone(),
            execution: execution.clone(),
            tools: Vec::new(),
            omitted_result_count: *omitted_result_count,
            internal_chat_message_metadata_passthrough: metadata.clone(),
        },
        _ => return None,
    })
}

fn truncated_output_payload(output: &FunctionCallOutputPayload) -> FunctionCallOutputPayload {
    FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string()),
        success: output.success,
    }
}

#[cfg(test)]
#[path = "compact_remote_tests.rs"]
mod tests;
