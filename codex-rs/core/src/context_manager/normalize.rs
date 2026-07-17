use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use std::collections::HashMap;
use std::collections::HashSet;
use uuid::Uuid;

use crate::util::error_or_panic;
use tracing::info;

const IMAGE_CONTENT_OMITTED_PLACEHOLDER: &str =
    "image content omitted because you do not support image input";
// Changing this value would change model-visible IDs and invalidate prompt caches.
const SYNTHETIC_OUTPUT_ID_NAMESPACE: Uuid = Uuid::from_u128(0x90d38d3e_6a5b_4d52_bfe2_2f1e634bfac4);

/// Pairing metadata used to normalize an append-only history without rescanning
/// every preceding item. Each raw item owns one normalized segment; appending a
/// matching call or output only invalidates the counterpart segments returned by
/// [`Self::affected_positions`].
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptNormalizationIndex {
    function_calls: HashMap<String, Vec<usize>>,
    local_shell_calls: HashMap<String, Vec<usize>>,
    function_outputs: HashMap<String, Vec<usize>>,
    tool_search_calls: HashMap<String, Vec<usize>>,
    tool_search_outputs: HashMap<String, Vec<usize>>,
    custom_tool_calls: HashMap<String, Vec<usize>>,
    custom_tool_outputs: HashMap<String, Vec<usize>>,
}

impl PromptNormalizationIndex {
    pub(crate) fn from_items(items: &[ResponseItem]) -> Self {
        let mut index = Self::default();
        for (position, item) in items.iter().enumerate() {
            index.insert(position, item);
        }
        index
    }

    pub(crate) fn insert(&mut self, position: usize, item: &ResponseItem) {
        let entry = match item {
            ResponseItem::FunctionCall { call_id, .. } => Some((&mut self.function_calls, call_id)),
            ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => Some((&mut self.local_shell_calls, call_id)),
            ResponseItem::FunctionCallOutput { call_id, .. } => {
                Some((&mut self.function_outputs, call_id))
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => Some((&mut self.tool_search_calls, call_id)),
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => Some((&mut self.tool_search_outputs, call_id)),
            ResponseItem::CustomToolCall { call_id, .. } => {
                Some((&mut self.custom_tool_calls, call_id))
            }
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                Some((&mut self.custom_tool_outputs, call_id))
            }
            _ => None,
        };
        if let Some((positions, call_id)) = entry {
            positions.entry(call_id.clone()).or_default().push(position);
        }
    }

    pub(crate) fn affected_positions(&self, item: &ResponseItem) -> Vec<usize> {
        match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => positions(&self.function_outputs, call_id),
            ResponseItem::FunctionCallOutput { call_id, .. } => {
                let mut affected = positions(&self.function_calls, call_id);
                affected.extend(positions(&self.local_shell_calls, call_id));
                affected
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => positions(&self.tool_search_outputs, call_id),
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => positions(&self.tool_search_calls, call_id),
            ResponseItem::CustomToolCall { call_id, .. } => {
                positions(&self.custom_tool_outputs, call_id)
            }
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                positions(&self.custom_tool_calls, call_id)
            }
            _ => Vec::new(),
        }
    }

    pub(crate) fn normalize_segment(
        &self,
        item: &ResponseItem,
        input_modalities: &[InputModality],
    ) -> Vec<ResponseItem> {
        let mut segment = match item {
            ResponseItem::FunctionCall { id, call_id, .. }
                if !self.function_outputs.contains_key(call_id) =>
            {
                vec![
                    item.clone(),
                    ResponseItem::FunctionCallOutput {
                        id: synthetic_output_id("fco", id.as_deref()),
                        call_id: call_id.clone(),
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ]
            }
            ResponseItem::LocalShellCall {
                id,
                call_id: Some(call_id),
                ..
            } if !self.function_outputs.contains_key(call_id) => vec![
                item.clone(),
                ResponseItem::FunctionCallOutput {
                    id: synthetic_output_id("fco", id.as_deref()),
                    call_id: call_id.clone(),
                    output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            ResponseItem::ToolSearchCall {
                id,
                call_id: Some(call_id),
                ..
            } if !self.tool_search_outputs.contains_key(call_id) => vec![
                item.clone(),
                ResponseItem::ToolSearchOutput {
                    id: synthetic_output_id("tso", id.as_deref()),
                    call_id: Some(call_id.clone()),
                    status: "completed".to_string(),
                    execution: "client".to_string(),
                    tools: Vec::new(),
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            ResponseItem::CustomToolCall { id, call_id, .. }
                if !self.custom_tool_outputs.contains_key(call_id) =>
            {
                error_or_panic(format!(
                    "Custom tool call output is missing for call id: {call_id}"
                ));
                vec![
                    item.clone(),
                    ResponseItem::CustomToolCallOutput {
                        id: synthetic_output_id("ctco", id.as_deref()),
                        call_id: call_id.clone(),
                        name: None,
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ]
            }
            ResponseItem::FunctionCallOutput { call_id, .. }
                if !self.function_calls.contains_key(call_id)
                    && !self.local_shell_calls.contains_key(call_id) =>
            {
                error_or_panic(format!(
                    "Orphan function call output for call id: {call_id}"
                ));
                Vec::new()
            }
            ResponseItem::CustomToolCallOutput { call_id, .. }
                if !self.custom_tool_calls.contains_key(call_id) =>
            {
                error_or_panic(format!(
                    "Orphan custom tool call output for call id: {call_id}"
                ));
                Vec::new()
            }
            ResponseItem::ToolSearchOutput { execution, .. } if execution == "server" => {
                vec![item.clone()]
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } if !self.tool_search_calls.contains_key(call_id) => {
                error_or_panic(format!("Orphan tool search output for call id: {call_id}"));
                Vec::new()
            }
            _ => vec![item.clone()],
        };
        strip_images_when_unsupported(input_modalities, &mut segment);
        segment
    }
}

fn positions(index: &HashMap<String, Vec<usize>>, call_id: &str) -> Vec<usize> {
    index.get(call_id).cloned().unwrap_or_default()
}

pub(crate) fn ensure_call_outputs_present(items: &mut Vec<ResponseItem>) {
    let mut function_output_ids = HashSet::new();
    let mut tool_search_output_ids = HashSet::new();
    let mut custom_tool_output_ids = HashSet::new();
    for item in items.iter() {
        match item {
            ResponseItem::FunctionCallOutput { call_id, .. } => {
                function_output_ids.insert(call_id.as_str());
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => {
                tool_search_output_ids.insert(call_id.as_str());
            }
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                custom_tool_output_ids.insert(call_id.as_str());
            }
            _ => {}
        }
    }

    // Collect synthetic outputs to insert immediately after their calls.
    // Store the insertion position (index of call) alongside the item so
    // we can insert in reverse order and avoid index shifting.
    let mut missing_outputs_to_insert: Vec<(usize, ResponseItem)> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        match item {
            ResponseItem::FunctionCall { id, call_id, .. }
                if !function_output_ids.contains(call_id.as_str()) =>
            {
                info!("Function call output is missing for call id: {call_id}");
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::FunctionCallOutput {
                        id: synthetic_output_id("fco", id.as_deref()),
                        call_id: call_id.clone(),
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            ResponseItem::ToolSearchCall {
                id,
                call_id: Some(call_id),
                ..
            } if !tool_search_output_ids.contains(call_id.as_str()) => {
                info!("Tool search output is missing for call id: {call_id}");
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::ToolSearchOutput {
                        id: synthetic_output_id("tso", id.as_deref()),
                        call_id: Some(call_id.clone()),
                        status: "completed".to_string(),
                        execution: "client".to_string(),
                        tools: Vec::new(),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            ResponseItem::CustomToolCall { id, call_id, .. }
                if !custom_tool_output_ids.contains(call_id.as_str()) =>
            {
                error_or_panic(format!(
                    "Custom tool call output is missing for call id: {call_id}"
                ));
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::CustomToolCallOutput {
                        id: synthetic_output_id("ctco", id.as_deref()),
                        call_id: call_id.clone(),
                        name: None,
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            // LocalShellCall is represented in upstream streams by a FunctionCallOutput
            ResponseItem::LocalShellCall {
                id,
                call_id: Some(call_id),
                ..
            } if !function_output_ids.contains(call_id.as_str()) => {
                error_or_panic(format!(
                    "Local shell call output is missing for call id: {call_id}"
                ));
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItem::FunctionCallOutput {
                        id: synthetic_output_id("fco", id.as_deref()),
                        call_id: call_id.clone(),
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ));
            }
            _ => {}
        }
    }
    drop((
        function_output_ids,
        tool_search_output_ids,
        custom_tool_output_ids,
    ));

    // Insert synthetic outputs in reverse index order to avoid re-indexing.
    for (idx, output_item) in missing_outputs_to_insert.into_iter().rev() {
        items.insert(idx + 1, output_item);
    }
}

/// Derives a stable ID for a prompt-only output from its source call's item ID.
///
/// Prompt normalization can run repeatedly without persisting its synthetic
/// outputs, so the namespace and name format must remain stable across retries
/// and resumes to preserve prompt-cache reuse. Returning `None` when the source
/// call has no ID preserves the legacy behavior for older history items.
fn synthetic_output_id(prefix: &str, item_id: Option<&str>) -> Option<String> {
    let source_id = item_id.filter(|id| !id.is_empty())?;
    let name = format!("{prefix}:{source_id}");
    Some(format!(
        "{prefix}_{}",
        Uuid::new_v5(&SYNTHETIC_OUTPUT_ID_NAMESPACE, name.as_bytes())
    ))
}

pub(crate) fn remove_orphan_outputs(items: &mut Vec<ResponseItem>) {
    let function_call_ids: HashSet<String> = items
        .iter()
        .filter_map(|i| match i {
            ResponseItem::FunctionCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    let tool_search_call_ids: HashSet<String> = items
        .iter()
        .filter_map(|i| match i {
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    let local_shell_call_ids: HashSet<String> = items
        .iter()
        .filter_map(|i| match i {
            ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    let custom_tool_call_ids: HashSet<String> = items
        .iter()
        .filter_map(|i| match i {
            ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    items.retain(|item| match item {
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            let has_match =
                function_call_ids.contains(call_id) || local_shell_call_ids.contains(call_id);
            if !has_match {
                error_or_panic(format!(
                    "Orphan function call output for call id: {call_id}"
                ));
            }
            has_match
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            let has_match = custom_tool_call_ids.contains(call_id);
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
            let has_match = tool_search_call_ids.contains(call_id);
            if !has_match {
                error_or_panic(format!("Orphan tool search output for call id: {call_id}"));
            }
            has_match
        }
        ResponseItem::ToolSearchOutput { call_id: None, .. } => true,
        _ => true,
    });
}

pub(crate) fn remove_corresponding_for(items: &mut Vec<ResponseItem>, item: &ResponseItem) {
    match item {
        ResponseItem::FunctionCall { call_id, .. } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::FunctionCallOutput {
                        call_id: existing, ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            if let Some(pos) = items.iter().position(|i| {
                matches!(i, ResponseItem::FunctionCall { call_id: existing, .. } if existing == call_id)
            }) {
                items.remove(pos);
            } else if let Some(pos) = items.iter().position(|i| {
                matches!(i, ResponseItem::LocalShellCall { call_id: Some(existing), .. } if existing == call_id)
            }) {
                items.remove(pos);
            }
        }
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::ToolSearchOutput {
                        call_id: Some(existing),
                        ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => {
            remove_first_matching(
                items,
                |i| {
                    matches!(
                        i,
                        ResponseItem::ToolSearchCall {
                            call_id: Some(existing),
                            ..
                        } if existing == call_id
                    )
                },
            );
        }
        ResponseItem::CustomToolCall { call_id, .. } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::CustomToolCallOutput {
                        call_id: existing, ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            remove_first_matching(
                items,
                |i| matches!(i, ResponseItem::CustomToolCall { call_id: existing, .. } if existing == call_id),
            );
        }
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::FunctionCallOutput {
                        call_id: existing, ..
                    } if existing == call_id
                )
            });
        }
        _ => {}
    }
}

fn remove_first_matching<F>(items: &mut Vec<ResponseItem>, predicate: F)
where
    F: Fn(&ResponseItem) -> bool,
{
    if let Some(pos) = items.iter().position(predicate) {
        items.remove(pos);
    }
}

/// Strip image content from messages and tool outputs when the model does not support images.
/// When `input_modalities` contains `InputModality::Image`, no stripping is performed.
pub(crate) fn strip_images_when_unsupported(
    input_modalities: &[InputModality],
    items: &mut [ResponseItem],
) {
    let supports_images = input_modalities.contains(&InputModality::Image);
    if supports_images {
        return;
    }

    for item in items.iter_mut() {
        match item {
            ResponseItem::Message { content, .. } => {
                let mut normalized_content = Vec::with_capacity(content.len());
                for content_item in content.iter() {
                    match content_item {
                        ContentItem::InputImage { .. } => {
                            normalized_content.push(ContentItem::InputText {
                                text: IMAGE_CONTENT_OMITTED_PLACEHOLDER.to_string(),
                            });
                        }
                        _ => normalized_content.push(content_item.clone()),
                    }
                }
                *content = normalized_content;
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content_items) = output.content_items_mut() {
                    let mut normalized_content_items = Vec::with_capacity(content_items.len());
                    for content_item in content_items.iter() {
                        match content_item {
                            FunctionCallOutputContentItem::InputImage { .. } => {
                                normalized_content_items.push(
                                    FunctionCallOutputContentItem::InputText {
                                        text: IMAGE_CONTENT_OMITTED_PLACEHOLDER.to_string(),
                                    },
                                );
                            }
                            _ => normalized_content_items.push(content_item.clone()),
                        }
                    }
                    *content_items = normalized_content_items;
                }
            }
            ResponseItem::ImageGenerationCall { result, .. } => {
                result.clear();
            }
            _ => {}
        }
    }
}
