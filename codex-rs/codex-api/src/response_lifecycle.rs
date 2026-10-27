//! Validate the sequential item contract before correlation fields are erased.
//! Unsupported overlapping lifecycles fail closed rather than mislabeling deltas.

use super::ResponsesEventError;
use super::ResponsesStreamEvent;
use super::parse_required_response_item;
use crate::common::ResponseEvent;
use crate::error::ApiError;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;

const MAX_ITEMS: usize = 4096;
const MAX_RETAINED_WIRE_BYTES: usize = 64 * 1024 * 1024;

/// Socket-lifetime ownership. Never evict identities while reusing the socket:
/// retire it at the bound instead of making an old response eligible again.
#[derive(Default)]
pub(crate) struct ConnectionResponseHistory {
    responses: HashSet<String>,
    items: HashSet<String>,
    identity_bytes: usize,
    ambiguous: bool,
}

impl ConnectionResponseHistory {
    pub(crate) fn must_retire(&self) -> bool {
        self.ambiguous
            || self.responses.len() + self.items.len() >= MAX_ITEMS
            || self.identity_bytes >= 1024 * 1024
    }
}

#[derive(Default)]
pub(super) struct ResponseLifecycle {
    response_id: Option<String>,
    active: Option<ItemIdentity>,
    active_wire_bytes: usize,
    active_fragments: HashMap<(FragmentKind, u64), String>,
    active_fragment_bytes: usize,
    admitted: Vec<ResponseItem>,
    admitted_output_indices: Vec<Option<u64>>,
    identities: HashMap<String, usize>,
    retained_wire_bytes: usize,
    connection: ConnectionResponseHistory,
    saw_unidentified_item: bool,
    completed_id: Option<String>,
    finalized_summaries: HashMap<i64, String>,
    finalized_summary_bytes: usize,
}

struct ItemIdentity {
    id: Option<String>,
    call_id: Option<String>,
    kind: String,
    output_index: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum FragmentKind {
    OutputText,
    Refusal,
    FunctionArguments,
    CustomInput,
    ReasoningText,
    ReasoningSummary,
}

fn finalized_fragment(item: &ResponseItem, key: (FragmentKind, u64)) -> Option<&str> {
    let (kind, index) = key;
    let index = usize::try_from(index).ok()?;
    match (item, kind) {
        (ResponseItem::Message { content, .. }, FragmentKind::OutputText) => match content.get(index)? {
            ContentItem::OutputText { text } => Some(text),
            _ => None,
        },
        (ResponseItem::Message { content, .. }, FragmentKind::Refusal) => match content.get(index)? {
            ContentItem::Refusal { refusal } => Some(refusal),
            _ => None,
        },
        (ResponseItem::FunctionCall { arguments, .. }, FragmentKind::FunctionArguments) if index == 0 => Some(arguments),
        (ResponseItem::CustomToolCall { input, .. }, FragmentKind::CustomInput) if index == 0 => Some(input),
        (ResponseItem::Reasoning { summary, .. }, FragmentKind::ReasoningSummary) => {
            let ReasoningItemReasoningSummary::SummaryText { text } = summary.get(index)?;
            Some(text)
        }
        (ResponseItem::Reasoning { content: Some(content), .. }, FragmentKind::ReasoningText) => match content.get(index)? {
            ReasoningItemContent::ReasoningText { text } | ReasoningItemContent::Text { text } => Some(text),
        },
        _ => None,
    }
}

fn invalid(message: &str) -> ResponsesEventError {
    ResponsesEventError::Api(ApiError::invalid_response(message))
}

impl ItemIdentity {
    fn from_event(event: &ResponsesStreamEvent<'_>) -> Option<Self> {
        let item = event.item.as_ref()?;
        let kind = item.get("type").and_then(Value::as_str)?;
        Some(Self {
            id: item.get("id").and_then(Value::as_str).map(str::to_owned),
            call_id: matches!(kind, "function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call" | "tool_search_output")
                .then(|| item.get("call_id").and_then(Value::as_str))
                .flatten()
                .map(str::to_owned),
            kind: kind.to_owned(),
            output_index: event.output_index,
        })
    }

    fn keys(&self) -> impl Iterator<Item = String> + '_ {
        self.id
            .iter()
            .map(|id| format!("item:{id}"))
            .chain(self.call_id.iter().map(|id| format!("call:{id}")))
            .chain(
                self.output_index
                    .iter()
                    .map(|index| format!("index:{index}")),
            )
    }

    fn matches(&self, other: &Self) -> bool {
        self.kind == other.kind
            && !matches!((&self.id, &other.id), (Some(a), Some(b)) if a != b)
            && !matches!((&self.call_id, &other.call_id), (Some(a), Some(b)) if a != b)
            && !matches!((self.output_index, other.output_index), (Some(a), Some(b)) if a != b)
    }
}

impl ResponseLifecycle {
    pub(super) fn owns_compaction_progress(&self, event: &ResponsesStreamEvent<'_>) -> bool {
        if event.kind != "response.compaction.compacting" {
            return false;
        }
        // The documented event carries item_id/output_index, not response_id.
        // Only an admitted compaction item's exact identity can extend its wait.
        self.active.as_ref().is_some_and(|active| {
            matches!(active.kind.as_str(), "compaction" | "context_compaction")
                && active.id.as_ref().is_some_and(|id| event.item_id.as_ref() == Some(id))
                && event.output_index.is_some_and(|index| {
                    active.output_index.is_none_or(|expected| index == expected)
                })
        })
    }

    fn append_fragment(&mut self, key: (FragmentKind, u64), text: &str) -> Result<(), ResponsesEventError> {
        if text.is_empty() {
            return Ok(());
        }
        let retained = self.retained_wire_bytes.saturating_add(self.active_wire_bytes)
            .saturating_add(self.finalized_summary_bytes).saturating_add(self.active_fragment_bytes);
        if (self.active_fragments.len() >= MAX_ITEMS && !self.active_fragments.contains_key(&key))
            || text.len() > MAX_RETAINED_WIRE_BYTES.saturating_sub(retained)
        {
            return Err(invalid("streamed output fragment budget exceeded"));
        }
        self.active_fragments.entry(key).or_default().push_str(text);
        self.active_fragment_bytes += text.len();
        Ok(())
    }

    fn seed_fragments(&mut self, item: &ResponseItem) -> Result<(), ResponsesEventError> {
        match item {
            ResponseItem::Message { content, .. } => {
                for (index, content) in content.iter().enumerate() {
                    match content {
                        ContentItem::OutputText { text } => self.append_fragment((FragmentKind::OutputText, index as u64), text)?,
                        ContentItem::Refusal { refusal } => self.append_fragment((FragmentKind::Refusal, index as u64), refusal)?,
                        _ => {}
                    }
                }
            }
            ResponseItem::FunctionCall { arguments, .. } => self.append_fragment((FragmentKind::FunctionArguments, 0), arguments)?,
            ResponseItem::CustomToolCall { input, .. } => self.append_fragment((FragmentKind::CustomInput, 0), input)?,
            ResponseItem::Reasoning { summary, content, .. } => {
                for (index, ReasoningItemReasoningSummary::SummaryText { text }) in summary.iter().enumerate() {
                    self.append_fragment((FragmentKind::ReasoningSummary, index as u64), text)?;
                }
                for (index, content) in content.iter().flatten().enumerate() {
                    let (ReasoningItemContent::ReasoningText { text } | ReasoningItemContent::Text { text }) = content;
                    self.append_fragment((FragmentKind::ReasoningText, index as u64), text)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn owns_terminal_observation(&self, id: Option<&str>) -> bool {
        id.is_some_and(|id| {
            !self.connection.responses.contains(id)
                && self
                    .response_id
                    .as_deref()
                    .is_none_or(|expected| expected == id)
        })
    }

    pub(super) fn with_connection_history(connection: ConnectionResponseHistory) -> Self {
        Self {
            connection,
            ..Self::default()
        }
    }

    pub(super) fn into_connection_history(mut self) -> ConnectionResponseHistory {
        // Legacy streams remain usable for a single request, but cannot prove
        // ownership across a reuse boundary without Created and item identities.
        self.connection.ambiguous |= self.response_id.is_none() || self.saw_unidentified_item;
        if let Some(id) = self.completed_id {
            self.connection.identity_bytes += id.len();
            self.connection.responses.insert(id);
        }
        for key in self
            .identities
            .into_keys()
            .filter(|key| !key.starts_with("index:"))
        {
            self.connection.identity_bytes += key.len();
            self.connection.items.insert(key);
        }
        self.connection
    }

    fn validate_ownership(
        &self,
        event: &ResponsesStreamEvent<'_>,
        identity: Option<&ItemIdentity>,
    ) -> Result<(), ResponsesEventError> {
        let supplied_response_id = event
            .response
            .as_ref()
            .and_then(|response| response.id.as_ref())
            .and_then(Value::as_str)
            .or(event.response_id.as_deref());
        if supplied_response_id.is_some_and(|id| self.connection.responses.contains(id))
            || identity
                .is_some_and(|item| item.keys().any(|key| self.connection.items.contains(&key)))
            || event
                .item_id
                .as_ref()
                .is_some_and(|id| self.connection.items.contains(&format!("item:{id}")))
        {
            return Err(invalid(
                "event belongs to a previous response on this connection",
            ));
        }
        if let (Some(expected), Some(actual)) = (self.response_id.as_deref(), supplied_response_id)
            && expected != actual
        {
            return Err(invalid("event has a different response id"));
        }
        Ok(())
    }

    pub(super) fn process(
        &mut self,
        mut event: ResponsesStreamEvent<'_>,
        wire_bytes: usize,
        completion_usage: Option<codex_protocol::protocol::TokenUsage>,
        ownership_admitted: &mut bool,
    ) -> Result<Option<ResponseEvent>, ResponsesEventError> {
        *ownership_admitted = false;
        let mut identity = ItemIdentity::from_event(&event);
        self.validate_ownership(&event, identity.as_ref())?;
        *ownership_admitted = true;
        if matches!(event.kind.as_ref(), "response.output_item.added" | "response.output_item.done")
            && event.item.as_ref().and_then(|item| item.get("id"))
            .is_some_and(|id| !id.is_null() && !id.is_string())
        {
            return Err(invalid("output item id must be a string or null"));
        }
        if !self.connection.responses.is_empty()
            && self.response_id.is_none()
            && matches!(
                event.kind.as_ref(),
                "response.output_item.added"
                    | "response.output_item.done"
                    | "response.output_text.delta"
                    | "response.refusal.delta"
                    | "response.refusal.done"
                    | "response.function_call_arguments.delta"
                    | "response.custom_tool_call_input.delta"
                    | "response.reasoning_text.delta"
                    | "response.reasoning_summary_text.delta"
            )
        {
            return Err(invalid(
                "reused connection received output before response.created",
            ));
        }
        match event.kind.as_ref() {
            "response.created" => {
                if self.response_id.is_some() || self.active.is_some() || !self.admitted.is_empty()
                {
                    return Err(invalid("response.created after response output started"));
                }
                self.response_id = event
                    .response
                    .as_ref()
                    .and_then(|response| response.id.as_ref())
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "response.output_item.added" => {
                if self.active.is_some() {
                    return Err(invalid("overlapping output items are not supported"));
                }
                if identity
                    .as_ref()
                    .is_some_and(|item| item.keys().any(|key| self.identities.contains_key(&key)))
                {
                    return Err(invalid("output item started again after finalization"));
                }
            }
            "response.completed" => {
                if self.active.is_some() {
                    return Err(invalid("response.completed with an unfinished output item"));
                }
                if let Some(response) = &mut event.response {
                    if let Some(expected) = &self.response_id
                        && response.id.as_ref().and_then(Value::as_str) != Some(expected.as_str())
                    {
                        return Err(invalid("response.completed has a different response id"));
                    }
                    if let Some(snapshot) = response.output.take() {
                        let Value::Array(snapshot) = snapshot else {
                            return Err(invalid("invalid terminal output snapshot"));
                        };
                        if snapshot.len() != self.admitted.len() {
                            return Err(invalid("terminal output does not match finalized items"));
                        }
                        for (item, admitted) in snapshot.into_iter().zip(&self.admitted) {
                            if parse_required_response_item(
                                "response.output_item.done",
                                Some(item),
                            )? != *admitted
                            {
                                return Err(invalid(
                                    "terminal output conflicts with finalized items",
                                ));
                            }
                        }
                    }
                }
            }
            kind if super::is_known_event(kind)
                && (kind.ends_with(".delta")
                    || kind == "response.refusal.done"
                    || kind.starts_with("response.reasoning_summary_")) =>
            {
                if event.summary_index.is_some_and(|index| index < 0)
                    || event.content_index.is_some_and(|index| index < 0)
                {
                    return Err(invalid("negative output content index"));
                }
                if let Some(active) = &self.active {
                    let expected_kind = match kind {
                        "response.output_text.delta" | "response.refusal.delta" | "response.refusal.done" => Some("message"),
                        "response.function_call_arguments.delta" => Some("function_call"),
                        "response.custom_tool_call_input.delta" => Some("custom_tool_call"),
                        kind if kind.starts_with("response.reasoning_") => Some("reasoning"),
                        _ => None,
                    };
                    if expected_kind.is_some_and(|kind| active.kind != kind)
                        || matches!((&event.item_id, &active.id), (Some(a), Some(b)) if a != b)
                        || matches!((&event.call_id, &active.call_id), (Some(a), Some(b)) if a != b)
                        || matches!((event.output_index, active.output_index), (Some(a), Some(b)) if a != b)
                    {
                        return Err(invalid("delta does not belong to the active output item"));
                    }
                } else {
                    return Err(invalid(
                        "output fragment received without an active output item",
                    ));
                }
            }
            _ => {}
        }
        if matches!(event.kind.as_ref(),
            "response.reasoning_summary_text.delta" | "response.reasoning_summary_part.added")
            && event.summary_index.is_some_and(|index| self.finalized_summaries.contains_key(&index))
        {
            return Err(invalid("reasoning summary fragment received after finalization"));
        }
        let fragment_key = match event.kind.as_ref() {
            "response.output_text.delta" => Some((FragmentKind::OutputText, event.content_index.unwrap_or(0) as u64)),
            "response.refusal.delta" => Some((FragmentKind::Refusal, event.content_index.unwrap_or(0) as u64)),
            "response.function_call_arguments.delta" => Some((FragmentKind::FunctionArguments, 0)),
            "response.custom_tool_call_input.delta" => Some((FragmentKind::CustomInput, 0)),
            "response.reasoning_text.delta" => event.content_index.map(|index| (FragmentKind::ReasoningText, index as u64)),
            "response.reasoning_summary_text.delta" => event.summary_index.map(|index| (FragmentKind::ReasoningSummary, index as u64)),
            _ => None,
        };
        if let (Some(key), Some(delta)) = (fragment_key, event.delta.as_deref()) {
            self.append_fragment(key, delta)?;
        }
        let finalized_key = match event.kind.as_ref() {
            "response.refusal.done" => Some((FragmentKind::Refusal, event.content_index.unwrap_or(0) as u64)),
            "response.reasoning_summary_text.done" => event.summary_index.map(|index| (FragmentKind::ReasoningSummary, index as u64)),
            _ => None,
        };
        if let (Some(key), Some(text)) = (finalized_key, event.text.as_deref()) {
            if let Some(streamed) = self.active_fragments.get(&key) {
                if streamed != text {
                    return Err(invalid("finalized output text conflicts with streamed fragments"));
                }
            } else {
                self.append_fragment(key, text)?;
            }
        }
        if event.kind == "response.reasoning_summary_text.done"
            && let (Some(index), Some(text), Some(_)) =
                (event.summary_index, event.text.as_ref(), event.item_id.as_ref())
        {
            if let Some(previous) = self.finalized_summaries.get(&index) {
                return if previous == text {
                    Ok(None)
                } else {
                    Err(invalid("conflicting reasoning summary finalization"))
                };
            }
            if self.finalized_summaries.len() >= MAX_ITEMS
                || text.len() > MAX_RETAINED_WIRE_BYTES.saturating_sub(self.finalized_summary_bytes)
            {
                return Err(invalid("reasoning summary finalization budget exceeded"));
            }
            self.finalized_summary_bytes += text.len();
            self.finalized_summaries.insert(index, text.clone());
        }
        if event.kind == "response.output_item.done"
            && let (Some(active), Some(done)) = (&self.active, identity.as_mut())
            && active.matches(done)
        {
            // Core used to fill an omitted done ID after client history had
            // already captured it. Null is also an absent optional ID in the
            // protocol schema; other JSON types were rejected above.
            if done.id.is_none()
                && let Some(id) = &active.id
            {
                done.id = Some(id.clone());
                if let Some(item) = event.item.as_mut().and_then(Value::as_object_mut) {
                    item.insert("id".into(), Value::String(id.clone()));
                }
            }
            done.output_index = done.output_index.or(active.output_index);
        }
        let result = super::process_responses_event(event, completion_usage)?;
        match &result {
            Some(ResponseEvent::OutputItemAdded(item)) => {
                self.finalized_summaries.clear();
                self.finalized_summary_bytes = 0;
                self.active = identity;
                self.active_wire_bytes = wire_bytes;
                self.active_fragments.clear();
                self.active_fragment_bytes = 0;
                self.seed_fragments(item)?;
            }
            Some(ResponseEvent::OutputItemDone(item)) => {
                // Final executable arguments and persisted text must be exactly
                // what the admitted seed/delta stream previewed. Final-only
                // fields remain compatible when no fragment was observed.
                if self.active.is_some() {
                    for (key, streamed) in &self.active_fragments {
                        // Raw reasoning plaintext is optional in the final item;
                        // absence is not an authoritative replacement snapshot.
                        if key.0 == FragmentKind::ReasoningText
                            && matches!(item, ResponseItem::Reasoning { content: None, .. })
                        {
                            continue;
                        }
                        if finalized_fragment(item, *key) != Some(streamed.as_str()) {
                            return Err(invalid("finalized output item conflicts with streamed fragments"));
                        }
                    }
                }
                if let ResponseItem::Reasoning { summary, .. } = item
                    && self.active.is_some()
                {
                    for (index, text) in &self.finalized_summaries {
                        let Some(codex_protocol::models::ReasoningItemReasoningSummary::SummaryText {
                            text: final_text,
                        }) = usize::try_from(*index).ok().and_then(|index| summary.get(index)) else {
                            return Err(invalid("final reasoning item omits a finalized summary"));
                        };
                        if final_text != text {
                            return Err(invalid("final reasoning item conflicts with a finalized summary"));
                        }
                    }
                }
                let keys = identity
                    .as_ref()
                    .map(|item| item.keys().collect::<Vec<_>>())
                    .unwrap_or_default();
                let mut replay = None;
                for key in &keys {
                    if let Some(index) = self.identities.get(key) {
                        if self.admitted[*index] != *item {
                            return Err(invalid("conflicting finalized output item identity"));
                        }
                        if matches!((self.admitted_output_indices[*index], identity.as_ref().and_then(|item| item.output_index)),
                            (Some(previous), Some(current)) if previous != current)
                        {
                            return Err(invalid("finalized output item changed output index"));
                        }
                        if replay.is_some_and(|previous| previous != *index) {
                            return Err(invalid("conflicting finalized output item aliases"));
                        }
                        replay = Some(*index);
                    }
                }
                let new_keys = keys.iter().filter(|key| !self.identities.contains_key(*key));
                let (new_count, new_bytes) = new_keys.fold((0usize, 0usize), |(count, bytes), key| {
                    (count.saturating_add(1), bytes.saturating_add(key.len()))
                });
                if self.identities.len().saturating_add(new_count) > MAX_ITEMS * 3
                    || new_bytes > MAX_RETAINED_WIRE_BYTES.saturating_sub(self.retained_wire_bytes)
                {
                    return Err(invalid("response item identity budget exceeded"));
                }
                if let Some(index) = replay {
                    self.admitted_output_indices[index] = self.admitted_output_indices[index]
                        .or(identity.as_ref().and_then(|item| item.output_index));
                    for key in keys {
                        self.identities.insert(key, index);
                    }
                    self.retained_wire_bytes += new_bytes;
                    return Ok(None);
                }
                if let (Some(active), Some(done)) = (&self.active, &identity)
                    && !active.matches(done)
                {
                    return Err(invalid(
                        "finalized item does not match the active output item",
                    ));
                }
                let charged_bytes = wire_bytes.saturating_add(self.active_wire_bytes).saturating_add(new_bytes);
                if self.admitted.len() >= MAX_ITEMS
                    || charged_bytes
                        > MAX_RETAINED_WIRE_BYTES.saturating_sub(self.retained_wire_bytes)
                {
                    return Err(invalid("response item admission budget exceeded"));
                }
                self.saw_unidentified_item |= !keys.iter().any(|key| !key.starts_with("index:"));
                for key in keys {
                    self.identities.insert(key, self.admitted.len());
                }
                self.retained_wire_bytes += charged_bytes;
                self.admitted_output_indices.push(identity.as_ref().and_then(|item| item.output_index));
                self.admitted.push(item.clone());
                self.active = None;
                self.active_wire_bytes = 0;
                self.active_fragments.clear();
                self.active_fragment_bytes = 0;
            }
            Some(ResponseEvent::Completed { response_id, .. }) => {
                self.completed_id = Some(response_id.clone());
            }
            _ => {}
        }
        Ok(result)
    }
}



