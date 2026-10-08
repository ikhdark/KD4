use std::collections::HashMap;
use std::collections::HashSet;

use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::compact::insert_compaction_initial_context;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::ContextManager;
use crate::context_manager::estimate_item_token_count;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tool_history::response_item_has_valid_tool_history_receipt;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;
use crate::tool_history::ToolSearchReceiptV1 as RemoteToolSearchReceiptV1;
use crate::tool_history::tool_search_receipt_id as remote_tool_search_receipt_id;
use sha2::Digest;
use sha2::Sha256;

const REMOTE_COMPACTION_TOOL_RECEIPT_MAX_TOKENS: usize = 2_000;
const REMOTE_COMPACTION_TOOL_RECEIPT_MAX_ITEMS: usize = 32;
const REMOTE_COMPACTION_TRANSPORT_RESERVE_TOKENS: i64 = 512;
const TOOL_SEARCH_RECEIPT_KIND: &str = "tool_search_receipt";
const TOOL_SEARCH_RECEIPT_MAX_TOKENS: usize = 256;
const TOOL_SEARCH_ARGUMENT_VALUE_MAX_TOKENS: usize = 64;

#[cfg(test)]
pub(crate) async fn process_compacted_history(
    sess: &Session,
    turn_context: &TurnContext,
    compacted_history: Vec<ResponseItem>,
    initial_context_injection: &InitialContextInjection,
) -> (
    Vec<ResponseItem>,
    Option<WorldStateSnapshot>,
    Vec<codex_protocol::protocol::ContextFragmentDigest>,
) {
    process_compacted_history_with_retained_input(
        sess,
        turn_context,
        compacted_history,
        Vec::new(),
        initial_context_injection,
    )
    .await
    .expect("test compaction must retain its artifacts")
}

pub(crate) async fn process_compacted_history_with_retained_input(
    sess: &Session,
    turn_context: &TurnContext,
    compacted_history: Vec<ResponseItem>,
    mut retained_input: Vec<ResponseItem>,
    initial_context_injection: &InitialContextInjection,
) -> codex_protocol::error::Result<(
    Vec<ResponseItem>,
    Option<WorldStateSnapshot>,
    Vec<codex_protocol::protocol::ContextFragmentDigest>,
)> {
    // Preserve the caller-selected replacement ordering. The default `AtStart` path retains the
    // cacheable prompt prefix while still leaving the summary or compaction item last.
    let (initial_context, world_state_baseline, fragment_digests) =
        build_compaction_initial_context(sess, turn_context, initial_context_injection).await;

    // Only provider output is subject to the consumed-transcript filter. Local
    // task context has already been selected against its own retention budget.
    let retained_input_len = retained_input.len();
    retained_input.extend(compacted_history);
    let history = sess.clone_history().await;
    let tool_history = history.tool_history_state();
    let mut reference_items = history.raw_items().to_vec();
    reference_items.extend(retained_input.iter().cloned());
    // Failure must reach the installer: the original history is not a new checkpoint.
    let artifact_pin_payload = sess
        .compaction_artifact_pins(&tool_history, &reference_items)
        .await?;
    // Recover exact registered references before the provider-output filter
    // removes consumed tool pairs. Their sidecar must survive that removal.
    let provider_output = retained_input.split_off(retained_input_len);
    retained_input.extend(bounded_remote_compacted_history(provider_output, |item| {
        tool_history.authenticates_receipt(item)
    }));
    let compacted_history =
        append_remote_compaction_artifact_pins(retained_input, artifact_pin_payload);
    Ok((
        insert_compaction_initial_context(
            compacted_history,
            initial_context,
            initial_context_injection,
        ),
        world_state_baseline,
        fragment_digests,
    ))
}

fn append_remote_compaction_artifact_pins(
    mut history: Vec<ResponseItem>,
    artifact_pin_payload: Option<String>,
) -> Vec<ResponseItem> {
    if let Some(text) = artifact_pin_payload {
        let mut item = crate::compact::compaction_context_message(text);
        crate::stable_context::mark_trusted_stable_context_item(&mut item);
        history.push(item);
    }
    history
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

fn bounded_remote_compacted_history(
    items: Vec<ResponseItem>,
    authenticates_receipt: impl Fn(&ResponseItem) -> bool,
) -> Vec<ResponseItem> {
    let receipt_index = tool_receipt_index(&items);
    let mut retained_indices = HashSet::new();
    let mut replacements = HashMap::new();
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
                let Some(group) = complete_tool_receipt_indices(&items, &receipt_index, index)
                else {
                    continue;
                };
                if group.iter().any(|index| retained_indices.contains(index)) {
                    continue;
                }
                if retained_tool_items.saturating_add(group.len())
                    > REMOTE_COMPACTION_TOOL_RECEIPT_MAX_ITEMS
                {
                    continue;
                }
                let search_receipt = remote_tool_search_receipt_group(&items, &group, true);
                if search_receipt.is_none()
                    && !group
                        .iter()
                        .any(|index| authenticates_receipt(&items[*index]))
                {
                    continue;
                }
                let item_tokens = |item: &ResponseItem| {
                    usize::try_from(estimate_item_token_count(item).max(1))
                        .unwrap_or(usize::MAX)
                };
                let tokens = search_receipt.as_ref().map_or_else(
                    || group.iter().fold(0usize, |total, index| {
                        total.saturating_add(item_tokens(&items[*index]))
                    }),
                    |(_, call, output)| item_tokens(call).saturating_add(item_tokens(output)),
                );
                if tokens <= remaining_tokens {
                    retained_tool_items = retained_tool_items.saturating_add(group.len());
                    remaining_tokens = remaining_tokens.saturating_sub(tokens);
                    retained_indices.extend(group.iter().copied());
                    if let Some((output_index, call, output)) = search_receipt
                        && let Some(call_index) = group
                            .iter()
                            .copied()
                            .find(|group_index| *group_index != output_index)
                    {
                        replacements.insert(call_index, call);
                        replacements.insert(output_index, output);
                    }
                }
            }
            _ => {}
        }
    }

    items
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| {
            retained_indices
                .contains(&index)
                .then(|| replacements.remove(&index).unwrap_or(item))
        })
        .collect()
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum ToolReceiptKind {
    Function,
    Custom,
    Search,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum ToolReceiptSide {
    Call,
    Output,
}

type ToolReceiptIndex<'a> = HashMap<(ToolReceiptKind, ToolReceiptSide, &'a str), usize>;

fn tool_receipt_index(items: &[ResponseItem]) -> ToolReceiptIndex<'_> {
    let mut index = HashMap::new();
    for (position, item) in items.iter().enumerate() {
        if let Some(identity) = tool_receipt_identity(item) {
            // Retain the first occurrence, matching the previous counterpart lookup.
            index.entry(identity).or_insert(position);
        }
    }
    index
}

fn complete_tool_receipt_indices(
    items: &[ResponseItem],
    receipt_index: &ToolReceiptIndex<'_>,
    index: usize,
) -> Option<Vec<usize>> {
    let (kind, side, call_id) = tool_receipt_identity(&items[index])?;
    let counterpart_side = match side {
        ToolReceiptSide::Call => ToolReceiptSide::Output,
        ToolReceiptSide::Output => ToolReceiptSide::Call,
    };
    let counterpart = *receipt_index.get(&(kind, counterpart_side, call_id))?;
    let mut group = vec![index, counterpart];
    group.sort_unstable();
    group.dedup();
    Some(group)
}

fn item_has_recoverable_artifact_reference(item: &ResponseItem) -> bool {
    response_item_has_valid_tool_history_receipt(item)
}

fn remote_tool_search_receipt_group(
    items: &[ResponseItem],
    group: &[usize],
    complete: bool,
) -> Option<(usize, ResponseItem, ResponseItem)> {
    let call = group.iter().find_map(|index| match &items[*index] {
        call @ ResponseItem::ToolSearchCall { .. } => Some(call),
        _ => None,
    })?;
    let (output_index, output) = group.iter().find_map(|index| match &items[*index] {
        output @ ResponseItem::ToolSearchOutput { .. } => Some((*index, output)),
        _ => None,
    })?;
    let ResponseItem::ToolSearchCall {
        id: call_item_id,
        call_id: Some(call_id),
        status: call_status,
        execution: call_execution,
        arguments,
        internal_chat_message_metadata_passthrough: call_metadata,
    } = call
    else {
        return None;
    };
    let ResponseItem::ToolSearchOutput {
        id: output_item_id,
        call_id: Some(output_call_id),
        status,
        execution,
        tools,
        omitted_result_count,
        internal_chat_message_metadata_passthrough: output_metadata,
    } = output
    else {
        return None;
    };
    if output_call_id != call_id {
        return None;
    }
    let prior_receipt = tools.first().and_then(parse_remote_tool_search_receipt);
    if tools.len() == 1
        && tools[0].get("type").and_then(serde_json::Value::as_str)
            == Some(TOOL_SEARCH_RECEIPT_KIND)
        && !prior_receipt.as_ref().is_some_and(|receipt| {
            remote_tool_search_receipt_is_valid(receipt, call_id, status, execution)
        })
    {
        return None;
    }
    let result_set_sha256 = match prior_receipt.as_ref() {
        Some(receipt) => receipt.result_set_sha256.clone(),
        None => format!("{:x}", Sha256::digest(serde_json::to_vec(tools).ok()?)),
    };
    let ordered_tool_identities = prior_receipt.as_ref().map_or_else(
        || {
            tools
                .iter()
                .filter_map(tool_search_result_identity)
                .collect()
        },
        |receipt| receipt.ordered_tool_identities.clone(),
    );
    let total_identity_count =
        prior_receipt
            .as_ref()
            .map_or(ordered_tool_identities.len(), |receipt| {
                receipt
                    .ordered_tool_identities
                    .len()
                    .saturating_add(receipt.omitted_identity_count)
            });
    let result_count = prior_receipt
        .as_ref()
        .map_or(tools.len(), |receipt| receipt.result_count);
    let receipt_arguments = prior_receipt.as_ref().map_or_else(
        || compact_tool_search_arguments(arguments),
        |receipt| receipt.arguments.clone(),
    );
    let prior_omitted_result_count = prior_receipt
        .as_ref()
        .and_then(|receipt| receipt.omitted_result_count)
        .or(*omitted_result_count);
    // Serialized size grows with the retained prefix: receipt IDs have fixed
    // width and each JSON string costs more than a decimal-count digit change.
    // Keep the largest fitting prefix without rebuilding every shorter prefix.
    let mut lower = 0;
    let mut upper = ordered_tool_identities.len();
    let mut retained = upper;
    let mut best = None;
    let receipt = loop {
        let complete = complete
            && prior_receipt
                .as_ref()
                .is_none_or(|receipt| receipt.complete)
            && status == "completed"
            && prior_omitted_result_count.unwrap_or(0) == 0;
        let omitted_identity_count =
            total_identity_count.saturating_sub(retained);
        let receipt_id = remote_tool_search_receipt_id(
            call_id,
            status,
            execution,
            &receipt_arguments,
            &result_set_sha256,
            result_count,
            prior_omitted_result_count,
            complete,
            omitted_identity_count,
            &ordered_tool_identities[..retained],
        );
        let receipt = RemoteToolSearchReceiptV1 {
            version: crate::tool_history::TOOL_SEARCH_RECEIPT_VERSION,
            receipt_id,
            call_id: call_id.clone(),
            status: status.clone(),
            execution: execution.clone(),
            arguments: receipt_arguments.clone(),
            result_set_sha256: result_set_sha256.clone(),
            result_count,
            omitted_result_count: prior_omitted_result_count,
            complete,
            ordered_tool_identities: ordered_tool_identities[..retained].to_vec(),
            omitted_identity_count,
        };
        let rendered = serde_json::to_string(&receipt).ok()?;
        if approx_token_count(&rendered) <= TOOL_SEARCH_RECEIPT_MAX_TOKENS {
            if retained == upper {
                break receipt;
            }
            best = Some(receipt);
            lower = retained + 1;
        } else if retained == 0 {
            return None;
        } else {
            upper = retained - 1;
        }
        if lower > upper {
            break best?;
        }
        retained = lower + (upper - lower) / 2;
    };
    let receipt_value = serde_json::to_value(&receipt).ok()?;
    let bounded_call = ResponseItem::ToolSearchCall {
        id: call_item_id.clone(),
        call_id: Some(call_id.clone()),
        status: call_status.clone(),
        execution: call_execution.clone(),
        arguments: receipt_arguments,
        internal_chat_message_metadata_passthrough: call_metadata.clone(),
    };
    let receipt_output = ResponseItem::ToolSearchOutput {
        id: output_item_id.clone(),
        call_id: Some(call_id.clone()),
        status: status.clone(),
        execution: execution.clone(),
        tools: vec![serde_json::json!({
            "type": TOOL_SEARCH_RECEIPT_KIND,
            "receipt": receipt_value,
        })],
        omitted_result_count: receipt.omitted_result_count,
        internal_chat_message_metadata_passthrough: output_metadata.clone(),
    };
    Some((output_index, bounded_call, receipt_output))
}

fn parse_remote_tool_search_receipt(
    value: &serde_json::Value,
) -> Option<RemoteToolSearchReceiptV1> {
    (value.get("type")?.as_str()? == TOOL_SEARCH_RECEIPT_KIND)
        .then(|| serde_json::from_value(value.get("receipt")?.clone()).ok())
        .flatten()
}

fn remote_tool_search_receipt_is_valid(
    receipt: &RemoteToolSearchReceiptV1,
    call_id: &str,
    status: &str,
    execution: &str,
) -> bool {
    receipt.is_valid(call_id, status, execution)
}

fn compact_tool_search_arguments(arguments: &serde_json::Value) -> serde_json::Value {
    let mut compact = serde_json::Map::new();
    for key in ["query", "namespace", "limit", "cursor"] {
        if let Some(value) = arguments.get(key) {
            let serialized = value.to_string();
            if approx_token_count(&serialized) > TOOL_SEARCH_ARGUMENT_VALUE_MAX_TOKENS {
                compact.insert(
                    format!("{key}_sha256"),
                    serde_json::Value::String(format!(
                        "{:x}",
                        Sha256::digest(serialized.as_bytes())
                    )),
                );
            } else {
                compact.insert(key.to_string(), value.clone());
            }
        }
    }
    if compact.is_empty() {
        serde_json::json!({
            "arguments_sha256": format!("{:x}", Sha256::digest(arguments.to_string().as_bytes()))
        })
    } else {
        serde_json::Value::Object(compact)
    }
}

fn tool_search_result_identity(tool: &serde_json::Value) -> Option<String> {
    let name = tool
        .get("name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
        })?;
    let namespace = tool
        .get("namespace")
        .and_then(serde_json::Value::as_str)
        .filter(|namespace| !namespace.is_empty());
    Some(match namespace {
        Some(namespace) => format!("{namespace}.{name}"),
        None => name.to_string(),
    })
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

#[cfg(test)]
pub(crate) fn trim_function_call_history_to_fit_context_window(
    history: &mut ContextManager,
    turn_context: &TurnContext,
    base_instructions: &BaseInstructions,
) -> (usize, i64) {
    trim_function_call_history_to_fit_context_window_for_prompt(
        history,
        turn_context,
        base_instructions,
        None,
        0,
    )
}

pub(crate) fn trim_function_call_history_to_fit_context_window_for_prompt(
    history: &mut ContextManager,
    turn_context: &TurnContext,
    base_instructions: &BaseInstructions,
    prepared_items: Option<&[ResponseItem]>,
    request_overhead_tokens: i64,
) -> (usize, i64) {
    let Some(context_window) = turn_context.model_context_window() else {
        return (0, 0);
    };
    let mut estimated_tokens =
        i128::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i128::MAX);
    estimated_tokens = estimated_tokens.saturating_add(i128::from(request_overhead_tokens));
    let measured_items = prepared_items.unwrap_or_else(|| history.raw_items());
    for item_tokens in measured_items.iter().map(estimate_item_token_count) {
        estimated_tokens = estimated_tokens.saturating_add(i128::from(item_tokens));
    }
    let estimated_tokens_before = estimated_tokens;
    let context_window =
        i128::from(context_window.saturating_sub(REMOTE_COMPACTION_TRANSPORT_RESERVE_TOKENS));
    if estimated_tokens <= context_window {
        return (0, 0);
    }
    let receipt_index = tool_receipt_index(history.raw_items());
    let prepared_outputs = prepared_items.map(|items| {
        items
            .iter()
            .filter_map(|item| {
                let (kind, side, call_id) = tool_receipt_identity(item)?;
                (side == ToolReceiptSide::Output).then_some(((kind, call_id), item))
            })
            .collect::<HashMap<_, _>>()
    });
    let mut replacements = Vec::new();

    for index in (0..history.raw_items().len()).rev() {
        if estimated_tokens <= context_window {
            break;
        }
        let raw_item = &history.raw_items()[index];
        let current_model_item = if let Some(prepared_outputs) = prepared_outputs.as_ref() {
            let Some((kind, side, call_id)) = tool_receipt_identity(raw_item) else {
                continue;
            };
            if side != ToolReceiptSide::Output {
                continue;
            }
            let Some(item) = prepared_outputs.get(&(kind, call_id)) else {
                // The prepared prompt already omitted this raw output, so replacing it cannot
                // reduce the request that is actually about to be sent.
                continue;
            };
            *item
        } else {
            raw_item
        };
        if item_has_recoverable_artifact_reference(current_model_item)
            || matches!(
                current_model_item,
                ResponseItem::ToolSearchOutput { tools, .. }
                    if tools.first().and_then(parse_remote_tool_search_receipt).is_some()
            )
        {
            continue;
        }
        let Some(rewritten_item) = rewritten_output_for_context_window(history.raw_items(), &receipt_index, index)
        else {
            continue;
        };
        let rewritten_tokens = estimate_item_token_count(&rewritten_item);
        let current_tokens = estimate_item_token_count(current_model_item);
        // A receipt has fixed metadata overhead. Small raw outputs can be
        // cheaper; never enlarge an already oversized compaction request.
        if rewritten_tokens >= current_tokens {
            continue;
        }
        estimated_tokens = estimated_tokens
            .saturating_sub(i128::from(current_tokens))
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

fn rewritten_output_for_context_window(
    items: &[ResponseItem],
    receipt_index: &ToolReceiptIndex<'_>,
    index: usize,
) -> Option<ResponseItem> {
    let item = items.get(index)?;
    Some(match item {
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => {
            let call_index = *receipt_index.get(&(
                ToolReceiptKind::Search, ToolReceiptSide::Call, call_id.as_str(),
            ))?;
            let (_, _, output) =
                remote_tool_search_receipt_group(items, &[call_index, index], false)?;
            output
        }
        _ => return None,
    })
}

#[cfg(test)]
#[path = "compact_remote_tests.rs"]
mod tests;
