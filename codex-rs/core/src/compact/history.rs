//! Selects and rebuilds bounded compaction history without performing I/O.
//!
//! Owns user/agent allocation, unresolved-call retention, summary message construction,
//! and omission receipts. The parent module retains model execution, durable recovery,
//! and installation of the replacement history.

use std::sync::Arc;

use super::COMPACT_IMAGE_OMISSION_MARKER;
use super::COMPACT_TASK_STATE_MAX_TOKENS;
use super::CompactedUserMessage;
use super::InitialContextInjection;
use super::MAX_RETAINED_USER_IMAGE_BYTES;
use super::MAX_RETAINED_USER_IMAGES;
use super::TaskInputCheckpoint;
use crate::context::is_legacy_compaction_warning_fragment;
use crate::context::is_startup_contextual_user_fragment;
use crate::stable_context::is_trusted_stable_context_item;
use crate::stable_context::project_compaction_context;
use codex_conversation::compact::compaction_summary_text;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use serde::Serialize;

pub(crate) use codex_conversation::compact::is_compaction_summary_item;

const COMPACTION_SUMMARY_ITEM_ID_BASE: &str = "msg_compaction_summary";
const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 16_000;
const COMPACT_AGENT_MESSAGE_MAX_TOKENS: usize = 8_000;

const COMPACT_TEXT_OMISSION_MARKER: &str = "codex_local_compaction_text_omission";

#[derive(Clone, Debug, Serialize)]
struct CompactionTextOmissionReceiptV1 {
    version: u8,
    kind: &'static str,
    role: &'static str,
    source_item_id: Option<String>,
    source_index: usize,
    recovery_selector: serde_json::Value,
    turn_id: Option<String>,
    original_tokens: usize,
    retained_tokens: usize,
    omitted_tokens: usize,
    unresolved: bool,
    /// Bound once the recovery artifact exists; `recovery_selector` is relative to it.
    artifact_id: Option<String>,
}

pub(crate) fn strip_compaction_startup_envelopes(
    items: impl Into<Arc<[ResponseItem]>>,
) -> Vec<ResponseItem> {
    filter_compaction_startup_envelopes(project_compaction_context(items.into()))
}

fn filter_compaction_startup_envelopes(
    items: impl IntoIterator<Item = ResponseItem>,
) -> Vec<ResponseItem> {
    items
        .into_iter()
        .filter_map(|item| {
            let trusted = is_trusted_stable_context_item(&item);
            match item {
                ResponseItem::Message {
                    id,
                    role,
                    mut content,
                    phase,
                    internal_chat_message_metadata_passthrough,
                } if role == "user" && trusted => {
                    content.retain(|part| {
                        !is_startup_contextual_user_fragment(part)
                            || is_selected_skill_fragment(part)
                    });
                    (!content.is_empty()).then_some(ResponseItem::Message {
                        id,
                        role,
                        content,
                        phase,
                        internal_chat_message_metadata_passthrough,
                    })
                }
                item => Some(item),
            }
        })
        .collect()
}

fn is_selected_skill_fragment(part: &ContentItem) -> bool {
    use crate::context::ContextualUserFragment;
    matches!(part, ContentItem::InputText { text } if crate::context::SkillInjection::matches_text(text))
}

fn is_selected_skill_item(item: &ResponseItem) -> bool {
    is_trusted_stable_context_item(item)
        && matches!(item, ResponseItem::Message { content, .. }
            if !content.is_empty() && content.iter().all(is_selected_skill_fragment))
}

pub fn content_items_to_text(content: &[ContentItem]) -> Option<String> {
    let mut pieces = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text }
            | ContentItem::OutputText { text }
            | ContentItem::Refusal { refusal: text } => {
                if !text.is_empty() {
                    pieces.push(text.as_str());
                }
            }
            ContentItem::InputImage { .. } => {}
        }
    }
    if pieces.is_empty() {
        None
    } else {
        Some(pieces.join("\n"))
    }
}

pub(crate) fn collect_user_messages(items: &[ResponseItem]) -> Vec<CompactedUserMessage> {
    collect_user_messages_with_indices(items)
        .into_iter()
        .map(|(_, message)| message)
        .collect()
}

fn collect_user_messages_with_indices(
    items: &[ResponseItem],
) -> Vec<(usize, CompactedUserMessage)> {
    let mut seen_task_state = false;
    let mut messages = items
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, item)| match item {
            ResponseItem::Message {
                id,
                role,
                content,
                internal_chat_message_metadata_passthrough,
                ..
            } if role == "user" => {
                if is_compaction_summary_item(item)
                    || is_selected_skill_item(item)
                    || is_compaction_text_bookkeeping_item(item)
                    || (item.turn_id().is_none()
                        && content.iter().any(is_legacy_compaction_warning_fragment)
                        && crate::event_mapping::is_contextual_user_message(item))
                {
                    return None;
                }
                // A fragment supersedes only older task-state fragments, never
                // other user text or images sharing the same message.
                let mut retained = content
                    .iter()
                    .rev()
                    .filter(|part| {
                        !is_trusted_stable_context_item(item)
                            || retain_latest_task_state_fragment(part, &mut seen_task_state)
                    })
                    .collect::<Vec<_>>();
                retained.reverse();
                if retained.is_empty() && !content.is_empty() {
                    return None;
                }
                // Compaction is not a UI projection: path/number wrappers are
                // part of the image's model-visible source identity.
                let content = retained
                    .into_iter()
                    .filter_map(|part| match part {
                        ContentItem::InputText { text } => Some(UserInput::Text {
                            text: text.clone(),
                            text_elements: Vec::new(),
                        }),
                        ContentItem::InputImage { image_url, detail } => Some(UserInput::Image {
                            image_url: image_url.clone(),
                            detail: *detail,
                        }),
                        ContentItem::OutputText { .. } | ContentItem::Refusal { .. } => None,
                    })
                    .collect();
                Some((
                    index,
                    CompactedUserMessage {
                        source_item_id: id.as_ref().map(ToString::to_string),
                        content,
                        internal_chat_message_metadata_passthrough:
                            internal_chat_message_metadata_passthrough.clone(),
                    },
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    messages.reverse();
    messages
}

/// Omission receipts and recovery sidecars written by an earlier compaction.
/// They are bookkeeping, not user text: budgeting them as requests squeezes
/// real input, so every later compaction would cut the same message again and
/// emit another receipt, sidecar and artifact.
fn is_compaction_text_bookkeeping_item(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    role == "user"
        && !content.is_empty()
        && is_trusted_stable_context_item(item)
        && content.iter().all(|part| {
            matches!(part, ContentItem::InputText { text } if text.starts_with('{')
            && serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| {
                value["kind"] == COMPACT_TEXT_OMISSION_MARKER
                    || (value["kind"] == "local_compaction_text_recovery"
                        && value["artifact_id"].is_string())
            }))
        })
}

/// Bind this compaction's omission receipts to the artifact that holds their
/// exact text. Receipts carried from earlier compactions already name their own.
pub(crate) fn new_omission_receipt_ids(
    retained: &[ResponseItem],
    source: &[ResponseItem],
) -> std::collections::HashSet<codex_protocol::ResponseItemId> {
    let previous = source
        .iter()
        .filter_map(ResponseItem::id)
        .collect::<std::collections::HashSet<_>>();
    retained
        .iter()
        .filter_map(|item| {
            let id = item.id()?;
            (is_trusted_stable_context_item(item) && !previous.contains(id)).then(|| id.clone())
        })
        .collect()
}

pub(crate) fn bind_omission_receipts_to_recovery(
    history: &mut [ResponseItem],
    sidecar: &str,
    created_receipt_ids: &std::collections::HashSet<codex_protocol::ResponseItemId>,
) {
    let Some(artifact_id) = serde_json::from_str::<serde_json::Value>(sidecar)
        .ok()
        .and_then(|value| value["artifact_id"].as_str().map(str::to_owned))
    else {
        return;
    };
    let bound = format!("\"artifact_id\":{}", serde_json::Value::String(artifact_id));
    for item in history.iter_mut() {
        if !is_trusted_stable_context_item(item)
            || !item.id().is_some_and(|id| created_receipt_ids.contains(id))
        {
            continue;
        }
        let ResponseItem::Message { content, .. } = item else {
            continue;
        };
        for part in content {
            let ContentItem::InputText { text } = part else {
                continue;
            };
            let unbound = serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| {
                value["kind"] == COMPACT_TEXT_OMISSION_MARKER
                    && value
                        .get("artifact_id")
                        .is_some_and(serde_json::Value::is_null)
            });
            if unbound {
                // A quoted key inside a JSON string is escaped, so this token is the field.
                *text = text.replacen("\"artifact_id\":null", &bound, 1);
            }
        }
    }
}

// Call in reverse chronological order, including within mixed-content messages.
fn retain_latest_task_state_fragment(part: &ContentItem, seen: &mut bool) -> bool {
    use crate::context::ContextualUserFragment;
    use crate::context::world_state::TaskState;
    if let ContentItem::InputText { text } = part
        && TaskState::matches_text(text)
    {
        return !std::mem::replace(seen, true);
    }
    true
}

fn unpaired_compaction_outputs<'a>(
    items: &'a [ResponseItem],
    pairs: &[Option<usize>],
    start: usize,
) -> std::collections::HashSet<(u8, &'a str)> {
    items[start..]
        .iter()
        .enumerate()
        .filter_map(|(offset, item)| {
            let (kind, call, id) = crate::tool_occurrence::identity(item)?;
            (!call && pairs[start + offset].is_none()).then_some((kind, id))
        })
        .collect()
}

pub(super) fn compaction_summary_items(items: &[ResponseItem]) -> Vec<ResponseItem> {
    // Expired advice is developer-only: it cannot change call pairing, model
    // boundaries, or summary identity. Select in the source index space before
    // cloning, then apply the history owner's exact advice-retention predicate.
    let end = items
        .iter()
        .rposition(is_compaction_model_generated_item)
        .map_or(0, |index| index + 1);
    let pairs = crate::tool_occurrence::pairs(items);
    let unpaired = unpaired_compaction_outputs(items, &pairs, end);
    crate::context_manager::current_turn_advice_items(items)
        .filter(|(index, item)| {
            // A tail phase checkpoint still retires consumed outputs above it;
            // a pinned summarizer input applies it as sampling did.
            (*index < end
                || is_compaction_summary_item(item)
                || crate::tool_history::phase_checkpoint_ids(item).is_some())
                && !pairs[*index].is_some_and(|paired| *index < end && paired >= end)
                && !crate::tool_occurrence::identity(item)
                    .is_some_and(|(kind, call, id)| call && unpaired.contains(&(kind, id)))
        })
        .map(|(_, item)| item.clone())
        .collect()
}

fn unresolved_compaction_start(items: &[ResponseItem]) -> usize {
    let base_start = items
        .iter()
        .rposition(is_compaction_model_generated_item)
        .map_or(0, |index| index.saturating_add(1));
    let pairs = crate::tool_occurrence::pairs(items);
    let unpaired = unpaired_compaction_outputs(items, &pairs, base_start);
    pairs[base_start..]
        .iter()
        .filter_map(|paired| *paired)
        .filter(|&index| index < base_start)
        // Ambiguous outstanding legacy calls have no authenticated pair. Keep
        // all possible calls rather than treating the output as consumed.
        .chain(
            items[..base_start]
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    let (kind, call, id) = crate::tool_occurrence::identity(item)?;
                    (call && unpaired.contains(&(kind, id))).then_some(index)
                }),
        )
        .min()
        .unwrap_or(base_start)
}

fn compaction_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id),
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => Some(call_id),
        _ => None,
    }
}

pub(super) fn compaction_output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some(call_id),
        _ => None,
    }
}

fn append_bounded_agent_messages_with_indices(
    items: &[ResponseItem],
    max_tokens: usize,
) -> (Vec<ResponseItem>, Vec<usize>) {
    let mut selected = Vec::new();
    let mut selected_indices = Vec::new();
    let mut remaining = max_tokens;
    for (index, item) in items.iter().enumerate().rev() {
        let ResponseItem::AgentMessage {
            id,
            author,
            recipient,
            content,
            internal_chat_message_metadata_passthrough,
        } = item
        else {
            continue;
        };
        let mut bounded_content = Vec::new();
        for part in content {
            match part {
                AgentMessageInputContent::InputText { text }
                    if remaining > 0 && !text.is_empty() =>
                {
                    let tokens = approx_token_count(text);
                    let text = if tokens <= remaining {
                        remaining = remaining.saturating_sub(tokens);
                        text.clone()
                    } else {
                        let text = truncate_text_to_token_ceiling(text, remaining);
                        remaining = 0;
                        text
                    };
                    if !text.is_empty() {
                        bounded_content.push(AgentMessageInputContent::InputText { text });
                    }
                }
                AgentMessageInputContent::EncryptedContent { encrypted_content }
                    if remaining > 0 && !encrypted_content.is_empty() =>
                {
                    let tokens = approx_token_count(encrypted_content);
                    if tokens <= remaining {
                        remaining = remaining.saturating_sub(tokens);
                        bounded_content.push(AgentMessageInputContent::EncryptedContent {
                            encrypted_content: encrypted_content.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        if !bounded_content.is_empty() {
            selected_indices.push(index);
            selected.push(ResponseItem::AgentMessage {
                id: id.clone(),
                author: author.clone(),
                recipient: recipient.clone(),
                content: bounded_content,
                internal_chat_message_metadata_passthrough:
                    internal_chat_message_metadata_passthrough.clone(),
            });
        }
        if remaining == 0 {
            break;
        }
    }
    selected.reverse();
    selected_indices.reverse();
    (selected, selected_indices)
}

pub(super) fn is_compaction_model_generated_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role == "assistant",
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::CompactionTrigger { .. }
        | ResponseItem::AdditionalTools { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Other => false,
    }
}

pub(super) fn compaction_image_omission_marker(count: usize) -> String {
    format!("{COMPACT_IMAGE_OMISSION_MARKER} Omitted image count: {count}.")
}

/// Generated sidecars are context, not a new user turn that expires selected skills.
pub(crate) fn compaction_context_message(text: String) -> ResponseItem {
    let mut item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    crate::stable_context::mark_trusted_stable_context_item(&mut item);
    item
}

/// Preserve the actual user requests and the latest
/// assistant handoff as well as the unconsumed tail. A model-generated boundary
/// means input was observed, not that the requested work was completed.
pub(crate) fn task_compaction_items(items: &[ResponseItem]) -> Vec<ResponseItem> {
    let unresolved_start = unresolved_compaction_start(items);
    let latest_assistant = items.iter().rposition(
        |item| matches!(item, ResponseItem::Message { role, .. } if role == "assistant"),
    );
    let latest_summary = items.iter().rposition(is_compaction_summary_item);
    let mut retained = strip_compaction_startup_envelopes(
        crate::context_manager::current_turn_advice_items(items)
            .filter(|(index, item)| {
                *index >= unresolved_start
                    // Native messages keep their role and provenance and still
                    // pass through the bounded agent allocator and exact omission
                    // accounting. Consumed does not mean an instruction expired.
                    // Admit all native directions conservatively: this selector
                    // has no authenticated session ancestry context.
                    || matches!(item, ResponseItem::AgentMessage { .. })
                    || crate::session::is_unified_exec_resume_invalidation(item)
                    || codex_rollout::is_rollout_reconstruction_notice(item)
                    || crate::stable_context::is_unsettled_tool_recovery_notice(item)
                    || Some(*index) == latest_assistant
                    || Some(*index) == latest_summary
                    || is_selected_skill_item(item)
                    || codex_conversation::user_decision::AcceptedUserDecision::is_native_snapshot(item)
                    || (matches!(item, ResponseItem::Message { role, .. } if role == "user")
                        && !is_compaction_summary_item(item))
            })
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>(),
    );
    let mut seen_task_state = false;
    retained.reverse();
    retained.retain_mut(|item| {
        let trusted = is_trusted_stable_context_item(item);
        if let ResponseItem::Message { role, content, .. } = item {
            content.reverse();
            content.retain(|part| {
                (role != "user"
                    || !trusted
                    || retain_latest_task_state_fragment(part, &mut seen_task_state))
                    && !(trusted
                        && matches!(part, ContentItem::InputText { text }
                if text.starts_with("<codex_internal_context source=\"compaction_plan\">")
                    // Stripped startup envelopes include the AGENTS.md body these diffs apply to.
                    || crate::context::world_state::AgentsMdState::is_update_text(text)
                    || is_artifact_pin_text(text)))
            });
            content.reverse();
            return !content.is_empty();
        }
        true
    });
    retained.reverse();
    retained
}

/// Earlier pin sidecars are superseded: each compaction rebuilds one current pin set from
/// every reference in the source history, including references inside earlier pin sets.
fn is_artifact_pin_text(text: &str) -> bool {
    text.starts_with('{')
        && text.contains("\"tool_history_artifact_pins\"")
        && serde_json::from_str::<serde_json::Value>(text)
            .is_ok_and(|value| value["kind"] == "tool_history_artifact_pins")
}

pub(crate) fn build_task_input_checkpoint_with_details(
    items: &[ResponseItem],
) -> TaskInputCheckpoint {
    let (mut history, retained_images, omitted_images, _, omitted_text) =
        build_bounded_input_history(task_compaction_items(items), true);
    if omitted_images > 0 {
        history.push(compaction_context_message(
            compaction_image_omission_marker(omitted_images),
        ));
    }
    TaskInputCheckpoint {
        items: history,
        retained_images,
        omitted_images,
        omitted_text,
    }
}

pub(super) fn build_bounded_input_history(
    unresolved: impl AsRef<[ResponseItem]>,
    retain_handoff: bool,
) -> (Vec<ResponseItem>, usize, usize, bool, bool) {
    let unresolved = unresolved.as_ref();
    // Recovery stores only messages. Compute its indexes from the same input
    // before tool calls are filtered, rather than treating source indexes as
    // positions in the compact recovery array.
    let mut recovery_count = 0usize;
    let recovery_indexes = unresolved
        .iter()
        .map(|item| {
            let index = recovery_count;
            recovery_count += usize::from(matches!(
                item,
                ResponseItem::Message { .. } | ResponseItem::AgentMessage { .. }
            ));
            index
        })
        .collect::<Vec<_>>();
    let (user_source_indices, messages): (Vec<_>, Vec<_>) =
        collect_user_messages_with_indices(unresolved)
            .into_iter()
            .unzip();
    let (user_items, retained_image_count, omitted_image_count, selected_user_indices) =
        append_bounded_user_messages(
            Vec::new(),
            &messages,
            COMPACT_USER_MESSAGE_MAX_TOKENS,
            MAX_RETAINED_USER_IMAGES,
            MAX_RETAINED_USER_IMAGE_BYTES,
        );
    let (agent_items, agent_source_indices) =
        append_bounded_agent_messages_with_indices(unresolved, COMPACT_AGENT_MESSAGE_MAX_TOKENS);
    let selected_agent_items = agent_source_indices
        .iter()
        .copied()
        .zip(agent_items.iter().cloned())
        .collect::<Vec<_>>();

    let selected_user_source_indices = selected_user_indices
        .iter()
        .map(|user_index| user_source_indices[*user_index])
        .collect::<Vec<_>>();
    let mut indexed_items = selected_user_source_indices
        .iter()
        .copied()
        .zip(user_items)
        .chain(selected_agent_items.iter().cloned())
        .collect::<Vec<_>>();

    let retained_tokens_by_index = indexed_items
        .iter()
        .map(|(index, item)| {
            (
                *index,
                response_item_text_tokens(item).saturating_add(agent_message_text_tokens(item)),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    // Old selectors remain exact in the mandatory recovery archive. Budget their
    // model-visible envelope together with new receipts, never outside the cap.
    let mut omission_receipts = unresolved
        .iter()
        .enumerate()
        .filter(|(_, item)| is_compaction_text_bookkeeping_item(item))
        .map(|(index, item)| (index, item.clone()))
        .collect::<Vec<_>>();
    let inherited_receipt_count = omission_receipts.len();
    // Preserve the runtime boundary in source order. Moving it after post-resume
    // command receipts could incorrectly invalidate a newly returned handle.
    indexed_items.extend(
        unresolved
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                crate::session::is_unified_exec_resume_invalidation(item)
                    || codex_rollout::is_rollout_reconstruction_notice(item)
                    || crate::stable_context::is_unsettled_tool_recovery_notice(item)
                    || is_selected_skill_item(item)
                    || codex_conversation::user_decision::AcceptedUserDecision::is_native_snapshot(
                        item,
                    )
            })
            // Provenance is retained verbatim in source order, outside the
            // model-visible text budget (history projection never sends it).
            .map(|(index, item)| (index, item.clone())),
    );
    let latest_assistant = unresolved.iter().rposition(
        |item| matches!(item, ResponseItem::Message { role, .. } if role == "assistant"),
    );
    let latest_summary = unresolved.iter().rposition(is_compaction_summary_item);
    for (source_index, item) in unresolved.iter().enumerate().filter(|(index, _)| {
        retain_handoff && (Some(*index) == latest_assistant || Some(*index) == latest_summary)
    }) {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        if role != "assistant" && !is_compaction_summary_item(item) {
            continue;
        }
        let Some(text) = content_items_to_text(content) else {
            continue;
        };
        let bounded = truncate_text_to_token_ceiling(&text, COMPACT_TASK_STATE_MAX_TOKENS);
        let mut retained = item.clone();
        if bounded != text
            && let ResponseItem::Message { content, .. } = &mut retained
        {
            *content = vec![if role == "assistant" {
                ContentItem::OutputText {
                    text: bounded.clone(),
                }
            } else {
                ContentItem::InputText {
                    text: bounded.clone(),
                }
            }];
        }
        indexed_items.push((source_index, retained));
        if bounded != text {
            omission_receipts.push((
                source_index,
                compaction_text_omission_receipt(
                    if role == "assistant" {
                        "assistant"
                    } else {
                        "user"
                    },
                    item.id().map(ToString::to_string),
                    source_index,
                    recovery_indexes[source_index],
                    item.turn_id().map(str::to_string),
                    approx_token_count(&text),
                    approx_token_count(&bounded),
                ),
            ));
        }
    }
    let mut omitted_user_text = false;
    for (user_index, message) in messages.iter().enumerate() {
        let source_index = user_source_indices[user_index];
        let original_tokens = compacted_user_message_text_tokens(message);
        let retained_tokens = retained_tokens_by_index
            .get(&source_index)
            .copied()
            .unwrap_or(0);
        if retained_tokens < original_tokens {
            omitted_user_text = true;
            omission_receipts.push((
                source_index,
                compaction_text_omission_receipt(
                    "user",
                    message.source_item_id.clone(),
                    source_index,
                    recovery_indexes[source_index],
                    message
                        .internal_chat_message_metadata_passthrough
                        .as_ref()
                        .and_then(|metadata| metadata.turn_id.clone()),
                    original_tokens,
                    retained_tokens,
                ),
            ));
        }
    }

    for (source_index, item) in unresolved.iter().enumerate() {
        let ResponseItem::AgentMessage {
            id,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            continue;
        };
        let original_tokens = agent_message_text_tokens(item);
        let retained_tokens = retained_tokens_by_index
            .get(&source_index)
            .copied()
            .unwrap_or(0);
        if retained_tokens < original_tokens {
            omission_receipts.push((
                source_index,
                compaction_text_omission_receipt(
                    "agent",
                    id.as_ref().map(ToString::to_string),
                    source_index,
                    recovery_indexes[source_index],
                    internal_chat_message_metadata_passthrough
                        .as_ref()
                        .and_then(|metadata| metadata.turn_id.clone()),
                    original_tokens,
                    retained_tokens,
                ),
            ));
        }
    }

    // Detailed provenance is useful, but its envelope must also fit a fixed
    // budget. Exact text remains recoverable from the mandatory sidecar.
    let new_omissions = omission_receipts.len() > inherited_receipt_count;
    let mut receipt_tokens = 0usize;
    let mut omitted_receipt_count = 0usize;
    let mut first_omitted_index = None;
    for (index, receipt) in omission_receipts {
        let tokens = response_item_text_tokens(&receipt);
        if receipt_tokens.saturating_add(tokens) <= 1024 {
            receipt_tokens += tokens;
            indexed_items.push((index, receipt));
        } else {
            omitted_receipt_count += 1;
            first_omitted_index.get_or_insert(index);
        }
    }
    // Evicted inherited metadata forces retention before either publication path
    // replaces history. /items contains the prior sidecars and exact selectors.
    let omitted_text = new_omissions || omitted_receipt_count > 0;
    if let Some(index) = first_omitted_index {
        indexed_items.push((
            index,
            compaction_context_message(
                serde_json::json!({
                    "kind": COMPACT_TEXT_OMISSION_MARKER,
                    "unresolved": true,
                    "additional_omitted_messages": omitted_receipt_count,
                    "artifact_id": serde_json::Value::Null,
                    "instruction": "Recover exact text from the compaction recovery artifact.",
                })
                .to_string(),
            ),
        ));
    }
    let pending_output_ids = unresolved
        .iter()
        .filter_map(compaction_output_call_id)
        .collect::<std::collections::HashSet<_>>();
    for (source_index, item) in unresolved.iter().enumerate() {
        let Some(call_id) = compaction_call_id(item).or_else(|| compaction_output_call_id(item))
        else {
            continue;
        };
        let has_pending_output = pending_output_ids.contains(call_id);
        if has_pending_output {
            indexed_items.push((source_index, item.clone()));
        }
    }
    indexed_items.sort_by_key(|(index, _)| *index);
    let history = indexed_items.into_iter().map(|(_, item)| item).collect();
    (
        history,
        retained_image_count,
        omitted_image_count,
        omitted_user_text,
        omitted_text,
    )
}

fn compacted_user_message_text_tokens(message: &CompactedUserMessage) -> usize {
    message
        .content
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(approx_token_count(text)),
            _ => None,
        })
        .fold(0usize, usize::saturating_add)
}

fn response_item_text_tokens(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::Message { content, .. } => content
            .iter()
            .filter_map(|item| match item {
                ContentItem::InputText { text }
                | ContentItem::OutputText { text }
                | ContentItem::Refusal { refusal: text } => Some(approx_token_count(text)),
                ContentItem::InputImage { .. } => None,
            })
            .fold(0usize, usize::saturating_add),
        _ => 0,
    }
}

fn agent_message_text_tokens(item: &ResponseItem) -> usize {
    let ResponseItem::AgentMessage { content, .. } = item else {
        return 0;
    };
    content
        .iter()
        .map(|item| match item {
            AgentMessageInputContent::InputText { text } => approx_token_count(text),
            AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                approx_token_count(encrypted_content)
            }
        })
        .fold(0usize, usize::saturating_add)
}

fn compaction_text_omission_receipt(
    role: &'static str,
    source_item_id: Option<String>,
    source_index: usize,
    recovery_index: usize,
    turn_id: Option<String>,
    original_tokens: usize,
    retained_tokens: usize,
) -> ResponseItem {
    let receipt = CompactionTextOmissionReceiptV1 {
        version: 1,
        kind: COMPACT_TEXT_OMISSION_MARKER,
        role,
        source_item_id,
        source_index,
        recovery_selector: serde_json::json!({"kind":"json_pointer", "pointer":format!("/items/{recovery_index}")}),
        turn_id,
        original_tokens,
        retained_tokens,
        omitted_tokens: original_tokens.saturating_sub(retained_tokens),
        unresolved: true,
        artifact_id: None,
    };
    compaction_context_message(match serde_json::to_string(&receipt) {
        Ok(text) => text,
        Err(error) => unreachable!("compaction omission receipt must serialize: {error}"),
    })
}

fn compaction_summary_item(summary_text: String) -> ResponseItem {
    compaction_summary_item_with_artifact_pins(summary_text, None)
}

pub(super) fn compaction_summary_item_with_artifact_pins(
    summary_text: String,
    artifact_pin_payload: Option<String>,
) -> ResponseItem {
    let mut content = vec![ContentItem::InputText { text: summary_text }];
    if let Some(text) = artifact_pin_payload {
        content.push(ContentItem::InputText { text });
    }
    ResponseItem::Message {
        id: Some(ResponseItemId::new(COMPACTION_SUMMARY_ITEM_ID_BASE)),
        role: "user".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(super) fn latest_summary_message(items: &[ResponseItem]) -> Option<&str> {
    items.iter().rev().find_map(compaction_summary_text)
}

fn history_after_latest_summary_is_user_only(items: &[ResponseItem]) -> bool {
    let Some(summary_index) = items.iter().rposition(is_compaction_summary_item) else {
        return false;
    };
    items[summary_index + 1..]
        .iter()
        .all(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
}

pub(super) fn can_reuse_previous_summary(items: &[ResponseItem], omitted_user_text: bool) -> bool {
    !omitted_user_text && history_after_latest_summary_is_user_only(items)
}

pub(crate) fn has_authoritative_task_state_context(items: &[ResponseItem]) -> bool {
    items.iter().any(|item| {
        is_trusted_stable_context_item(item)
            && matches!(item,
        ResponseItem::Message { content, .. } if content.iter().any(|part| matches!(part,
            ContentItem::InputText { text } if text.starts_with("<codex_task_state>"))))
    })
}

pub(crate) fn insert_compaction_initial_context(
    mut compacted_history: Vec<ResponseItem>,
    mut initial_context: Vec<ResponseItem>,
    initial_context_injection: &InitialContextInjection,
) -> Vec<ResponseItem> {
    // A new owner snapshot supersedes earlier inventories, including an empty
    // snapshot. Never retain a stale running observation beside the new one.
    compacted_history
        .retain(|item| !crate::unified_exec::process_inventory::is_process_inventory(item));
    if has_authoritative_task_state_context(&initial_context) {
        compacted_history.retain_mut(|item| {
            if !is_trusted_stable_context_item(item) {
                return true;
            }
            let ResponseItem::Message { content, .. } = item else {
                return true;
            };
            content.retain(|part| {
                !matches!(part, ContentItem::InputText { text }
                if text.starts_with("<codex_task_state>")
                    || text.starts_with("<codex_internal_context source=\"compaction_plan\">"))
            });
            !content.is_empty()
        });
    }
    match initial_context_injection {
        InitialContextInjection::AtStart(_) => {
            initial_context.extend(compacted_history);
            initial_context
        }
    }
}

pub(crate) fn build_compacted_history(
    initial_context: Vec<ResponseItem>,
    user_messages: &[CompactedUserMessage],
    summary_text: &str,
) -> Vec<ResponseItem> {
    let (mut history, _retained_image_count, omitted_image_count, _selected_indices) =
        append_bounded_user_messages(
            initial_context,
            user_messages,
            COMPACT_USER_MESSAGE_MAX_TOKENS,
            MAX_RETAINED_USER_IMAGES,
            MAX_RETAINED_USER_IMAGE_BYTES,
        );

    let mut summary_text = if summary_text.is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.to_string()
    };
    if omitted_image_count > 0 {
        summary_text.push_str("\n\n");
        summary_text.push_str(&compaction_image_omission_marker(omitted_image_count));
    }

    history.push(compaction_summary_item(summary_text));

    history
}

fn append_bounded_user_messages(
    mut history: Vec<ResponseItem>,
    user_messages: &[CompactedUserMessage],
    max_tokens: usize,
    max_images: usize,
    max_image_bytes: usize,
) -> (Vec<ResponseItem>, usize, usize, Vec<usize>) {
    let mut selected_messages: Vec<(usize, CompactedUserMessage)> = Vec::new();
    let mut remaining = max_tokens;
    // Reserve bounded exact request/correction anchors before bulk payloads.
    // Allocation is not semantic classification: no keywords, summary call,
    // or assumption that the newest message supersedes earlier constraints.
    let costs: Vec<(usize, usize)> = user_messages
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .fold((0usize, 0usize), |(total, short), item| {
                    let UserInput::Text { text, .. } = item else {
                        return (total, short);
                    };
                    let cost = approx_token_count(text);
                    (
                        total.saturating_add(cost),
                        short.saturating_add(if cost <= 512 { cost } else { 0 }),
                    )
                })
        })
        .collect();
    let fits = costs
        .iter()
        .fold(0usize, |sum, (cost, _)| sum.saturating_add(*cost))
        <= max_tokens;
    let mut budgets: Vec<usize> = costs
        .iter()
        .map(|(cost, _)| if fits { *cost } else { 0 })
        .collect();
    if !fits {
        // Preserve early anchors, but do not let saturated older notes consume
        // the share reserved for recent short corrections.
        let mut anchors = max_tokens / 2;
        // Always reserve the original request before later short corrections.
        if let Some((cost, _)) = costs.first() {
            let budget = (*cost).min(512).min(anchors);
            budgets[0] = budget;
            anchors -= budget;
            remaining -= budget;
        }
        // The latest text input must not disappear when it crosses the short
        // part threshold. Reserve its bounded anchor before older short parts
        // spend the remaining allowance; trailing image-only inputs do not
        // displace it. This stays within the existing anchor and total budgets.
        if let Some(index) = costs
            .iter()
            .rposition(|(cost, _)| *cost > 0)
            .filter(|index| *index != 0)
        {
            let budget = costs[index].0.min(512).min(anchors);
            budgets[index] = budget;
            anchors -= budget;
            remaining -= budget;
        }
        // Short exact requests/corrections take priority over excerpts of bulk
        // payloads. Keep allocation deterministic and independent of prose.
        let anchor_order = costs
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, (cost, _))| *cost <= 512)
            .chain(
                costs
                    .iter()
                    .enumerate()
                    .skip(1)
                    .filter(|(_, (cost, _))| *cost > 512),
            );
        for (index, (cost, _)) in anchor_order {
            let budget = (*cost).min(512).saturating_sub(budgets[index]).min(anchors);
            budgets[index] += budget;
            anchors -= budget;
            remaining -= budget;
        }
        for (index, (_, demand)) in costs.iter().enumerate().rev() {
            let extra = demand.saturating_sub(budgets[index]).min(remaining);
            budgets[index] += extra;
            remaining -= extra;
        }
        for (index, (cost, _)) in costs.iter().enumerate().rev() {
            let extra = cost.saturating_sub(budgets[index]).min(remaining);
            budgets[index] += extra;
            remaining -= extra;
        }
    }
    let mut retained_image_count = 0usize;
    let mut retained_image_bytes = 0usize;
    let mut omitted_image_count = 0usize;
    for (index, message) in user_messages.iter().enumerate().rev() {
        let mut remaining = budgets[index];
        let mut short_remaining = costs[index].1;
        let mut content = Vec::new();
        for (part_index, item) in message.content.iter().enumerate() {
            if compacted_image_notice_index(&message.content, part_index).is_some() {
                continue;
            }
            // Keep recognized wrappers atomic with their image. Literal,
            // unbound image-like user text still follows normal text policy.
            if (part_index + 1 < message.content.len()
                && compacted_image_binding(&message.content, part_index + 1).is_some())
                || (part_index > 0
                    && compacted_image_binding(&message.content, part_index - 1).is_some())
            {
                continue;
            }
            match item {
                UserInput::Text { text, .. } if remaining > 0 && !text.is_empty() => {
                    let tokens = approx_token_count(text);
                    let allowance = if tokens <= 512 {
                        short_remaining = short_remaining.saturating_sub(tokens);
                        remaining
                    } else {
                        remaining.saturating_sub(short_remaining)
                    };
                    if allowance == 0 {
                        continue;
                    }
                    if tokens <= allowance {
                        content.push(UserInput::Text {
                            text: text.clone(),
                            text_elements: Vec::new(),
                        });
                        remaining = remaining.saturating_sub(tokens);
                    } else {
                        let truncated = truncate_text_to_token_ceiling(text, allowance);
                        if !truncated.is_empty() {
                            content.push(UserInput::Text {
                                text: truncated,
                                text_elements: Vec::new(),
                            });
                        }
                        remaining -= allowance;
                    }
                }
                UserInput::Image { image_url, detail } => {
                    let binding = compacted_image_binding(&message.content, part_index);
                    let notice_index = part_index + if binding.is_some() { 2 } else { 1 };
                    let notice = message
                        .content
                        .get(notice_index)
                        .and_then(|item| match item {
                            UserInput::Text { text, .. }
                                if crate::image_preparation::is_image_preparation_notice(text) =>
                            {
                                Some(text)
                            }
                            _ => None,
                        });
                    let binding_tokens = binding
                        .map_or(0, |(open, close)| {
                            approx_token_count(open).saturating_add(approx_token_count(close))
                        })
                        .saturating_add(notice.map_or(0, |text| approx_token_count(text)));
                    let binding_bytes = binding
                        .map_or(0, |(open, close)| open.len().saturating_add(close.len()))
                        .saturating_add(notice.map_or(0, String::len));
                    let next_bytes = retained_image_bytes
                        .saturating_add(image_url.len())
                        .saturating_add(binding_bytes);
                    if retained_image_count < max_images
                        && next_bytes <= max_image_bytes
                        && binding_tokens <= remaining
                    {
                        if let Some((open, _)) = binding {
                            content.push(UserInput::Text {
                                text: open.to_string(),
                                text_elements: Vec::new(),
                            });
                        }
                        content.push(UserInput::Image {
                            image_url: image_url.clone(),
                            detail: *detail,
                        });
                        if let Some((_, close)) = binding {
                            content.push(UserInput::Text {
                                text: close.to_string(),
                                text_elements: Vec::new(),
                            });
                        }
                        if let Some(text) = notice {
                            content.push(UserInput::Text {
                                text: text.clone(),
                                text_elements: Vec::new(),
                            });
                        }
                        remaining -= binding_tokens;
                        retained_image_count = retained_image_count.saturating_add(1);
                        retained_image_bytes = next_bytes;
                    } else {
                        omitted_image_count += 1;
                    }
                }
                _ => {}
            }
        }
        if !content.is_empty() {
            selected_messages.push((
                index,
                CompactedUserMessage {
                    source_item_id: message.source_item_id.clone(),
                    content,
                    internal_chat_message_metadata_passthrough: message
                        .internal_chat_message_metadata_passthrough
                        .clone(),
                },
            ));
        }
    }
    selected_messages.reverse();

    let mut selected_indices = Vec::with_capacity(selected_messages.len());
    for (index, message) in selected_messages {
        selected_indices.push(index);
        let content = message
            .content
            .into_iter()
            .filter_map(|item| match item {
                UserInput::Text { text, .. } => Some(ContentItem::InputText { text }),
                UserInput::Image { image_url, detail } => {
                    Some(ContentItem::InputImage { image_url, detail })
                }
                _ => None,
            })
            .collect();
        history.push(ResponseItem::Message {
            id: message
                .source_item_id
                .map(codex_protocol::ResponseItemId::from_server),
            role: "user".to_string(),
            content,
            phase: None,
            internal_chat_message_metadata_passthrough: message
                .internal_chat_message_metadata_passthrough,
        });
    }

    (
        history,
        retained_image_count,
        omitted_image_count,
        selected_indices,
    )
}

fn compacted_image_binding(content: &[UserInput], index: usize) -> Option<(&str, &str)> {
    use codex_protocol::models::is_image_close_tag_text;
    use codex_protocol::models::is_image_open_tag_text;
    use codex_protocol::models::is_local_image_open_tag_text;
    if !matches!(content.get(index), Some(UserInput::Image { .. })) {
        return None;
    }
    let UserInput::Text { text: open, .. } = content.get(index.checked_sub(1)?)? else {
        return None;
    };
    let UserInput::Text { text: close, .. } = content.get(index + 1)? else {
        return None;
    };
    ((is_local_image_open_tag_text(open) || is_image_open_tag_text(open))
        && is_image_close_tag_text(close))
    .then_some((open.as_str(), close.as_str()))
}

fn compacted_image_notice_index(content: &[UserInput], index: usize) -> Option<usize> {
    let UserInput::Text { text, .. } = content.get(index)? else {
        return None;
    };
    if !crate::image_preparation::is_image_preparation_notice(text) {
        return None;
    }
    if index > 0 && matches!(content[index - 1], UserInput::Image { .. }) {
        return Some(index - 1);
    }
    (index > 1 && compacted_image_binding(content, index - 2).is_some()).then(|| index - 2)
}
