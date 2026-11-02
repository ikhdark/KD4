//! Host-authored native clarification provenance. Tool output and quoted receipts
//! are never inputs to this owner. Like native user-message IDs, this namespace
//! is assigned only at the accepted input boundary, not inferred from prose.
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputDisposition;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputResponse;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

const OPEN: &str = "<codex_accepted_user_decision>";
const CLOSE: &str = "</codex_accepted_user_decision>";
const ID_PREFIX: &str = "msg_user_decision_";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedUserDecision {
    version: u8,
    pub turn_id: String,
    pub call_id: String,
    pub questions: Vec<RequestUserInputQuestion>,
    pub answers: BTreeMap<String, Vec<String>>,
}

impl AcceptedUserDecision {
    /// Call only after the native pending-request owner matches a live waiter.
    /// Negative dispositions, empty answers and secrets cannot grant plan scope.
    pub fn snapshot(
        turn_id: &str,
        call_id: &str,
        args: &RequestUserInputArgs,
        response: &RequestUserInputResponse,
    ) -> Result<Option<ResponseItem>, String> {
        if response.interrupted
            || turn_id.is_empty() || call_id.is_empty()
            || args.questions.is_empty() || args.questions.len() > 3
            || response.disposition.as_ref().is_some_and(|value|
                *value != RequestUserInputDisposition::Answered)
            || response.answers.is_empty()
            || args.questions.iter().any(|question| question.is_secret)
            || response.answers.keys().any(|id|
                !args.questions.iter().any(|question| &question.id == id))
            || !response.answers.values().any(|answer|
                answer.answers.iter().any(|text| !text.trim().is_empty()))
        {
            return Ok(None);
        }
        let decision = Self {
            version: 1,
            turn_id: turn_id.to_owned(),
            call_id: call_id.to_owned(),
            questions: args.questions.clone(),
            answers: response.answers.iter()
                .map(|(id, answer)| (id.clone(), answer.answers.clone())).collect(),
        };
        let body = serde_json::to_string(&decision).map_err(|error| error.to_string())?;
        let mut item = ResponseItem::Message {
            id: Some(ResponseItemId::new("msg_user_decision")),
            role: "developer".into(),
            content: vec![ContentItem::InputText { text: format!("{OPEN}{body}{CLOSE}") }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        item.set_turn_id_if_missing(turn_id);
        Ok(Some(item))
    }

    /// Historical text-only snapshots deliberately remain unauthenticated.
    pub fn from_item(item: &ResponseItem) -> Option<Self> {
        let ResponseItem::Message { id: Some(id), role, content, .. } = item else {
            return None;
        };
        if role != "developer" || !id.as_str().starts_with(ID_PREFIX) {
            return None;
        }
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return None;
        };
        let body = text.strip_prefix(OPEN)?.strip_suffix(CLOSE)?;
        let decision: Self = serde_json::from_str(body).ok()?;
        (decision.version == 1
            && item.turn_id() == Some(decision.turn_id.as_str())
            && !decision.call_id.is_empty()
            && !decision.questions.is_empty()
            && decision.questions.len() <= 3
            && decision.questions.iter().all(|question| !question.is_secret)
            && decision.answers.keys().all(|id|
                decision.questions.iter().any(|question| &question.id == id)))
            .then_some(decision)
    }

    pub fn is_native_snapshot(item: &ResponseItem) -> bool {
        matches!(item, ResponseItem::Message { id: Some(id), role, .. }
            if role == "developer" && id.as_str().starts_with(ID_PREFIX))
    }
}
