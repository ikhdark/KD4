//! Validate the sequential item contract before correlation fields are erased.
//! Unsupported overlapping lifecycles fail closed rather than mislabeling deltas.

use super::ResponsesEventError;
use super::ResponsesStreamEvent;
use super::parse_required_response_item;
use crate::common::ResponseEvent;
use crate::error::ApiError;
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
    admitted: Vec<ResponseItem>,
    identities: HashMap<String, usize>,
    retained_wire_bytes: usize,
    connection: ConnectionResponseHistory,
    saw_unidentified_item: bool,
    completed_id: Option<String>,
}

struct ItemIdentity {
    id: Option<String>,
    call_id: Option<String>,
    kind: String,
    output_index: Option<u64>,
}

fn invalid(message: &str) -> ResponsesEventError {
    ResponsesEventError::Api(ApiError::invalid_response(message))
}

impl ItemIdentity {
    fn from_event(event: &ResponsesStreamEvent<'_>) -> Option<Self> {
        let item = event.item.as_ref()?;
        Some(Self {
            id: item.get("id").and_then(Value::as_str).map(str::to_owned),
            call_id: item
                .get("call_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind: item.get("type").and_then(Value::as_str)?.to_owned(),
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

    pub(super) fn process(
        &mut self,
        mut event: ResponsesStreamEvent<'_>,
        wire_bytes: usize,
    ) -> Result<Option<ResponseEvent>, ResponsesEventError> {
        let mut identity = ItemIdentity::from_event(&event);
        let supplied_response_id = event
            .response
            .as_ref()
            .and_then(|response| response.id.as_ref())
            .and_then(Value::as_str)
            .or(event.response_id.as_deref());
        if supplied_response_id.is_some_and(|id| self.connection.responses.contains(id))
            || identity
                .as_ref()
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
        if !self.connection.responses.is_empty()
            && self.response_id.is_none()
            && matches!(
                event.kind.as_ref(),
                "response.output_item.added"
                    | "response.output_item.done"
                    | "response.output_text.delta"
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
                if let Some(response) = &event.response {
                    if let Some(expected) = &self.response_id
                        && response.id.as_ref().and_then(Value::as_str) != Some(expected.as_str())
                    {
                        return Err(invalid("response.completed has a different response id"));
                    }
                    if let Some(snapshot) = &response.output {
                        let snapshot = snapshot
                            .as_array()
                            .ok_or_else(|| invalid("invalid terminal output snapshot"))?;
                        if snapshot.len() != self.admitted.len() {
                            return Err(invalid("terminal output does not match finalized items"));
                        }
                        for (item, admitted) in snapshot.iter().zip(&self.admitted) {
                            if parse_required_response_item(
                                "response.output_item.done",
                                Some(item.clone()),
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
                    || kind.starts_with("response.reasoning_summary_")) =>
            {
                if event.summary_index.is_some_and(|index| index < 0)
                    || event.content_index.is_some_and(|index| index < 0)
                {
                    return Err(invalid("negative output content index"));
                }
                if let Some(active) = &self.active {
                    let expected_kind = match kind {
                        "response.output_text.delta" | "response.refusal.delta" => Some("message"),
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
        if event.kind == "response.output_item.done"
            && let (Some(active), Some(done)) = (&self.active, identity.as_mut())
            && active.matches(done)
        {
            // Core used to fill an omitted done ID after client history had
            // already captured it. Normalize once before either consumer.
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
        let result = super::process_responses_event(event)?;
        match &result {
            Some(ResponseEvent::OutputItemAdded(_)) => {
                self.active = identity;
                self.active_wire_bytes = wire_bytes;
            }
            Some(ResponseEvent::OutputItemDone(item)) => {
                let keys = identity
                    .as_ref()
                    .map(|item| item.keys().collect::<Vec<_>>())
                    .unwrap_or_default();
                let mut replay = false;
                for key in &keys {
                    if let Some(index) = self.identities.get(key) {
                        if self.admitted[*index] != *item {
                            return Err(invalid("conflicting finalized output item identity"));
                        }
                        replay = true;
                    }
                }
                if replay {
                    return Ok(None);
                }
                if let (Some(active), Some(done)) = (&self.active, &identity)
                    && !active.matches(done)
                {
                    return Err(invalid(
                        "finalized item does not match the active output item",
                    ));
                }
                let charged_bytes = wire_bytes.saturating_add(self.active_wire_bytes);
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
                self.admitted.push(item.clone());
                self.active = None;
                self.active_wire_bytes = 0;
            }
            Some(ResponseEvent::Completed { response_id, .. }) => {
                self.completed_id = Some(response_id.clone());
            }
            _ => {}
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ResponsesEventInterpreter;
    use super::super::ResponsesStreamMetadata;
    use super::*;
    use serde_json::json;

    fn interpreter() -> ResponsesEventInterpreter {
        ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None)
    }

    fn send(
        interpreter: &mut ResponsesEventInterpreter,
        value: Value,
    ) -> Result<Vec<ResponseEvent>, ResponsesEventError> {
        interpreter
            .process_payload(&value.to_string())
            .map(Iterator::collect)
    }

    fn message(id: &str, text: &str) -> Value {
        json!({"id":id,"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
    }

    #[test]
    fn initial_fragments_require_an_active_owner_even_after_created() {
        for created in [false, true] {
            for fragment in [
                json!({"type":"response.output_text.delta","item_id":"m","delta":"lost"}),
                json!({"type":"response.function_call_arguments.delta","item_id":"f","delta":"{}"}),
                json!({"type":"response.custom_tool_call_input.delta","item_id":"c","delta":"input"}),
                json!({"type":"response.reasoning_text.delta","content_index":0,"delta":"lost"}),
                json!({"type":"response.reasoning_summary_text.delta","summary_index":0,"delta":"lost"}),
                json!({"type":"response.reasoning_summary_text.done","summary_index":0,"text":"lost"}),
                json!({"type":"response.reasoning_summary_part.added","summary_index":0}),
            ] {
                let mut stream = interpreter();
                if created {
                    send(
                        &mut stream,
                        json!({"type":"response.created","response":{"id":"r"}}),
                    )
                    .unwrap();
                }
                let error = send(&mut stream, fragment).unwrap_err();
                let ResponsesEventError::Api(error) = error else {
                    panic!("expected protocol error")
                };
                assert!(!crate::api_bridge::map_api_error(error).is_retryable());
            }
        }
    }

    #[test]
    fn completion_rejects_unfinished_message_and_tool_with_or_without_snapshot() {
        for item in [
            message("m", ""),
            json!({"id":"f","type":"function_call","name":"marker","call_id":"c","arguments":""}),
        ] {
            for snapshot in [None, Some(vec![item.clone()])] {
                let mut stream = interpreter();
                send(
                    &mut stream,
                    json!({"type":"response.output_item.added","item":item}),
                )
                .unwrap();
                assert!(
                    send(
                        &mut stream,
                        json!({"type":"response.completed","response":{"id":"r","output":snapshot}})
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn terminal_snapshot_must_match_admitted_final_items() {
        for snapshot in [json!([]), json!([message("m", "different")])] {
            let mut stream = interpreter();
            send(
                &mut stream,
                json!({"type":"response.output_item.done","item":message("m", "answer")}),
            )
            .unwrap();
            assert!(
                send(
                    &mut stream,
                    json!({"type":"response.completed","response":{"id":"r","output":snapshot}})
                )
                .is_err()
            );
        }
        let mut stream = interpreter();
        send(
            &mut stream,
            json!({"type":"response.created","response":{"id":"r"}}),
        )
        .unwrap();
        send(
            &mut stream,
            json!({"type":"response.output_item.done","item":message("m", "answer")}),
        )
        .unwrap();
        assert!(matches!(send(&mut stream, json!({"type":"response.completed","response":{"id":"r","output":[message("m", "answer")]}})).unwrap().as_slice(), [ResponseEvent::Completed { .. }]));
    }

    #[test]
    fn replay_is_canonical_before_client_and_core_diverge() {
        for item in [
            message("m", "answer"),
            json!({"id":"f","type":"function_call","name":"marker","call_id":"c","arguments":"{}"}),
        ] {
            let mut stream = interpreter();
            let event = json!({"type":"response.output_item.done","item":item});
            assert_eq!(send(&mut stream, event.clone()).unwrap().len(), 1);
            assert!(send(&mut stream, event).unwrap().is_empty());
            assert_eq!(stream.lifecycle.admitted.len(), 1);
        }
        let mut stream = interpreter();
        for id in ["a", "b"] {
            assert_eq!(
                send(
                    &mut stream,
                    json!({"type":"response.output_item.done","item":message(id, "same text")})
                )
                .unwrap()
                .len(),
                1
            );
        }
        assert!(
            send(
                &mut stream,
                json!({"type":"response.output_item.done","item":message("a", "conflict")})
            )
            .is_err()
        );
    }

    #[test]
    fn overlapping_and_foreign_item_events_fail_before_mislabeling_output() {
        for next in [
            json!({"type":"response.output_item.added","item":{"id":"b","type":"reasoning","summary":[],"encrypted_content":null}}),
            json!({"type":"response.output_text.delta","item_id":"b","delta":"wrong"}),
            json!({"type":"response.reasoning_text.delta","item_id":"a","content_index":0,"delta":"wrong kind"}),
            json!({"type":"response.output_item.done","item":message("b", "wrong final")}),
        ] {
            let mut stream = interpreter();
            send(
                &mut stream,
                json!({"type":"response.output_item.added","item":message("a", "")}),
            )
            .unwrap();
            assert!(send(&mut stream, next).is_err());
        }
        let mut stream = interpreter();
        send(
            &mut stream,
            json!({"type":"response.output_item.added","item":message("a", "")}),
        )
        .unwrap();
        send(
            &mut stream,
            json!({"type":"response.output_text.delta","item_id":"a","delta":"answer"}),
        )
        .unwrap();
        send(
            &mut stream,
            json!({"type":"response.output_item.done","item":message("a", "answer")}),
        )
        .unwrap();
        assert!(
            send(
                &mut stream,
                json!({"type":"response.output_text.delta","item_id":"a","delta":"late"})
            )
            .is_err()
        );
    }

    #[test]
    fn created_and_completed_must_name_the_same_response() {
        let mut stream = interpreter();
        send(
            &mut stream,
            json!({"type":"response.created","response":{"id":"a"}}),
        )
        .unwrap();
        assert!(
            send(
                &mut stream,
                json!({"type":"response.completed","response":{"id":"b"}})
            )
            .is_err()
        );
    }

    #[test]
    fn omitted_final_id_is_normalized_before_history_and_snapshot_validation() {
        let mut stream = interpreter();
        send(
            &mut stream,
            json!({"type":"response.output_item.added","output_index":0,
            "item":message("m", "")}),
        )
        .unwrap();
        let events = send(&mut stream, json!({"type":"response.output_item.done","output_index":0,
            "item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}})).unwrap();
        assert!(matches!(&events[0], ResponseEvent::OutputItemDone(item)
            if serde_json::to_value(item).unwrap()["id"] == "m"));
        assert_eq!(stream.lifecycle.admitted.len(), 1);
        send(
            &mut stream,
            json!({"type":"response.completed","response":{"id":"r",
            "output":[message("m", "answer")]}}),
        )
        .unwrap();
    }

    #[test]
    fn unknown_extension_does_not_change_item_lifecycle() {
        let mut stream = interpreter();
        send(
            &mut stream,
            json!({"type":"response.output_item.done","item":message("m", "answer")}),
        )
        .unwrap();
        assert!(
            send(
                &mut stream,
                json!({"type":"provider.future.delta","item_id":"unrelated","delta":"extension"})
            )
            .unwrap()
            .is_empty()
        );
        send(
            &mut stream,
            json!({"type":"response.completed","response":{"id":"r"}}),
        )
        .unwrap();
    }
}
