use codex_protocol::ResponseItemId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use uuid::Uuid;

use crate::invariants::error_or_panic;
use crate::tool_occurrence;
use tracing::info;

pub(super) const MISSING_TOOL_RESULT: &str = "Result unavailable. Execution outcome and side effects are unknown. Do not repeat a state-changing operation solely because its result is missing.";
// Changing this value would change model-visible IDs and invalidate prompt caches.
const SYNTHETIC_OUTPUT_ID_NAMESPACE: Uuid = Uuid::from_u128(0x90d38d3e_6a5b_4d52_bfe2_2f1e634bfac4);

fn missing_tool_result(call_id: &str) -> FunctionCallOutputPayload {
    let mut failure = codex_protocol::tool_failure::ToolFailure::new(
        codex_protocol::tool_failure::ToolFailureCode::UncertainEffects,
        MISSING_TOOL_RESULT,
    );
    failure.details = Some(serde_json::json!({"call_id": call_id, "response_missing": true}));
    FunctionCallOutputPayload::from_text(failure.model_message())
}

pub(crate) fn ensure_call_outputs_present(items: &mut Vec<ResponseItem>) {
    let missing_outputs_to_insert = collect_missing_call_outputs(items, MissingOutputUse::Prompt);
    insert_items_after(items, missing_outputs_to_insert);

    // Make the historical result key explicit on the replay copy. Never
    // rewrite the stored item ID or add a dispatchable shell tool.
    for item in items {
        if let Some(call_id) = tool_occurrence::legacy_local_shell_item_id(item).map(str::to_owned) {
            tool_occurrence::set_call_id(item, call_id);
        }
    }
}

/// For a hypothetical prompt: a call still executing legitimately has no output yet,
/// which is only an invariant violation in a prompt that will actually be sent.
pub(crate) fn pair_in_flight_calls_for_estimate(items: &mut std::sync::Arc<Vec<ResponseItem>>) {
    let missing_outputs_to_insert = collect_missing_call_outputs(items, MissingOutputUse::Estimate);
    if !missing_outputs_to_insert.is_empty() {
        insert_items_after(std::sync::Arc::make_mut(items), missing_outputs_to_insert);
    }
}

/// Insert after original indexes in ascending order, without repeatedly moving
/// the history suffix. The common zero/one-insertion cases reuse the allocation.
fn insert_items_after(items: &mut Vec<ResponseItem>, mut insertions: Vec<(usize, ResponseItem)>) {
    if insertions.len() <= 1 {
        if let Some((index, item)) = insertions.pop() {
            items.insert(index + 1, item);
        }
        return;
    }
    let mut merged = Vec::with_capacity(items.len() + insertions.len());
    let mut insertions = insertions.into_iter().peekable();
    for (index, item) in std::mem::take(items).into_iter().enumerate() {
        merged.push(item);
        while insertions.peek().is_some_and(|(after, _)| *after == index) {
            if let Some((_, item)) = insertions.next() {
                merged.push(item);
            }
        }
    }
    debug_assert!(insertions.next().is_none());
    *items = merged;
}

pub fn missing_call_outputs(items: &[ResponseItem]) -> Vec<ResponseItem> {
    collect_missing_call_outputs(items, MissingOutputUse::Durable)
        .into_iter()
        .map(|(_, output)| output)
        .collect()
}

enum MissingOutputUse {
    Prompt,
    Estimate,
    Durable,
}

fn collect_missing_call_outputs(
    items: &[ResponseItem],
    usage: MissingOutputUse,
) -> Vec<(usize, ResponseItem)> {
    let historical = !matches!(usage, MissingOutputUse::Durable);
    let report_prompt_invariant_violations = matches!(usage, MissingOutputUse::Prompt);
    let paired = if historical {
        tool_occurrence::pairs(items)
    } else {
        tool_occurrence::explicit_pairs(items)
    };

    // Collect synthetic outputs to insert immediately after their calls.
    // Store original call indexes in ascending order for a single merge pass.
    let mut missing_outputs_to_insert: Vec<(usize, ResponseItem)> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        match item {
            ResponseItem::FunctionCall { id, call_id, .. } if paired[idx].is_none() => {
                info!(target: "codex_core::context_manager::normalize", "Function call output is missing for call id: {call_id}");
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::FunctionCallOutput {
                        id: synthetic_output_id("fco", id.as_deref()),
                        call_id: call_id.clone(),
                        output: missing_tool_result(call_id),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            ResponseItem::ToolSearchCall {
                id,
                call_id: Some(call_id),
                ..
            } if paired[idx].is_none() => {
                info!(target: "codex_core::context_manager::normalize", "Tool search output is missing for call id: {call_id}");
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::ToolSearchOutput {
                        id: synthetic_output_id("tso", id.as_deref()),
                        call_id: Some(call_id.clone()),
                        status: "incomplete".to_string(),
                        execution: "client".to_string(),
                        tools: Vec::new(),
                        omitted_result_count: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            ResponseItem::CustomToolCall { id, call_id, .. } if paired[idx].is_none() => {
                if report_prompt_invariant_violations {
                    error_or_panic(format!(
                        "Custom tool call output is missing for call id: {call_id}"
                    ));
                }
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::CustomToolCallOutput {
                        id: synthetic_output_id("ctco", id.as_deref()),
                        call_id: call_id.clone(),
                        name: None,
                        output: missing_tool_result(call_id),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            // LocalShellCall uses a FunctionCallOutput, including in legacy replay.
            ResponseItem::LocalShellCall {
                id,
                call_id: explicit_id,
                ..
            } if paired[idx].is_none() => {
                let identity = if historical {
                    tool_occurrence::history_identity(item)
                } else {
                    tool_occurrence::identity(item)
                };
                let Some((_, _, call_id)) = identity else {
                    continue;
                };
                // A missing historical result is unknown, not a violation of
                // the live dispatch owner's output invariant.
                if report_prompt_invariant_violations && explicit_id.is_some() {
                    error_or_panic(format!(
                        "Local shell call output is missing for call id: {call_id}"
                    ));
                }
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::FunctionCallOutput {
                        id: synthetic_output_id("fco", id.as_deref()),
                        call_id: call_id.to_owned(),
                        output: missing_tool_result(call_id),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            _ => {}
        }
    }
    missing_outputs_to_insert
}

/// Derives a stable ID for a prompt-only output from its source call's item ID.
///
/// Prompt normalization can run repeatedly without persisting its synthetic
/// outputs, so the namespace and name format must remain stable across retries
/// and resumes to preserve prompt-cache reuse. Returning `None` when the source
/// call has no ID preserves the legacy behavior for older history items.
fn synthetic_output_id(prefix: &str, item_id: Option<&str>) -> Option<ResponseItemId> {
    let source_id = item_id.filter(|id| !id.is_empty())?;
    let name = format!("{prefix}:{source_id}");
    Some(ResponseItemId::with_suffix(
        prefix,
        Uuid::new_v5(&SYNTHETIC_OUTPUT_ID_NAMESPACE, name.as_bytes()),
    ))
}

pub(crate) fn remove_orphan_outputs(items: &mut Vec<ResponseItem>) {
    let paired = tool_occurrence::pairs(items);

    // Find unmatched output occurrences before mutating their positions.
    let orphan_indexes: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let keep = match item {
                ResponseItem::FunctionCallOutput { call_id, .. } => {
                    let has_match = paired[index].is_some();
                    if !has_match {
                        error_or_panic(format!(
                            "Orphan function call output for call id: {call_id}"
                        ));
                    }
                    has_match
                }
                ResponseItem::CustomToolCallOutput { call_id, .. } => {
                    let has_match = paired[index].is_some();
                    if !has_match {
                        error_or_panic(format!(
                            "Orphan custom tool call output for call id: {call_id}"
                        ));
                    }
                    has_match
                }
                ResponseItem::ToolSearchOutput { execution, .. } if execution == "server" => true,
                ResponseItem::ToolSearchOutput {
                    call_id: Some(call_id),
                    ..
                } => {
                    let has_match = paired[index].is_some();
                    if !has_match {
                        error_or_panic(format!("Orphan tool search output for call id: {call_id}"));
                    }
                    has_match
                }
                ResponseItem::ToolSearchOutput { call_id: None, .. } => true,
                _ => true,
            };
            (!keep).then_some(index)
        })
        .collect();
    if orphan_indexes.is_empty() {
        return;
    }
    let mut orphan_indexes = orphan_indexes.into_iter().peekable();
    let mut index = 0;
    items.retain(|_| {
        let keep = orphan_indexes.peek() != Some(&index);
        if !keep {
            orphan_indexes.next();
        }
        index += 1;
        keep
    });
}

