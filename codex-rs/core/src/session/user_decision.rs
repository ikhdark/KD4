//! Authenticated UI decisions use the existing plan witness and rollout owners.

use super::*;
use crate::plan_store::AcceptedPlanInput;
use codex_protocol::models::ContentItem;
use codex_protocol::request_user_input::RequestUserInputDisposition;
use serde::Deserialize;
use serde::Serialize;

const PREFIX: &str = "<codex_accepted_user_decision>";
const SUFFIX: &str = "</codex_accepted_user_decision>";

/// Canonical, versioned source identity. A label alone cannot identify a decision.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcceptedUserDecision {
    version: u8,
    session_id: ThreadId,
    turn_id: String,
    call_id: String,
    request: RequestUserInputArgs,
    // HashMap iteration order must not affect the receipt digest.
    answers: BTreeMap<String, Vec<String>>,
    disposition: RequestUserInputDisposition,
}

impl AcceptedUserDecision {
    fn new(
        session_id: ThreadId,
        turn_id: &str,
        call_id: &str,
        request: RequestUserInputArgs,
        response: &RequestUserInputResponse,
    ) -> Option<Self> {
        // Legacy/non-answer dispositions do not establish explicit approval.
        if response.interrupted
            || response.disposition != Some(RequestUserInputDisposition::Answered)
        {
            return None;
        }
        let decision = Self {
            version: 1,
            session_id,
            turn_id: turn_id.into(),
            call_id: call_id.into(),
            request,
            answers: response
                .answers
                .iter()
                .map(|(id, answer)| (id.clone(), answer.answers.clone()))
                .collect(),
            disposition: RequestUserInputDisposition::Answered,
        };
        decision.is_valid(session_id).then_some(decision)
    }

    fn is_valid(&self, session_id: ThreadId) -> bool {
        self.version == 1
            && self.session_id == session_id
            && !self.turn_id.is_empty()
            && !self.call_id.is_empty()
            && self.disposition == RequestUserInputDisposition::Answered
            && !self.request.questions.is_empty()
            && self.answers.len() == self.request.questions.len()
            && self.request.questions.iter().all(|question| {
                self.answers.get(&question.id).is_some_and(|answers| {
                    !answers.is_empty() && answers.iter().all(|answer| !answer.trim().is_empty())
                })
            })
    }

    fn canonical_text(&self) -> std::io::Result<String> {
        serde_json::to_string(self).map_err(std::io::Error::other)
    }

    pub(super) fn accepted_input(&self) -> Option<AcceptedPlanInput> {
        let text = self.canonical_text().ok()?;
        Some(AcceptedPlanInput::from_texts(
            &self.turn_id,
            [text.as_str()].into_iter(),
        ))
    }

    pub(super) fn is_snapshot(item: &ResponseItem) -> bool {
        matches!(item, ResponseItem::Message { role, content, .. } if role == "developer"
            && matches!(content.as_slice(), [ContentItem::InputText { text }]
                if text.starts_with(PREFIX) && text.ends_with(SUFFIX)))
    }

    pub(super) fn from_snapshot(item: &ResponseItem, session_id: ThreadId) -> Option<Self> {
        if !Self::is_snapshot(item) {
            return None;
        }
        let ResponseItem::Message { content, .. } = item else {
            return None;
        };
        let ContentItem::InputText { text } = &content[0] else {
            return None;
        };
        let decision: Self =
            serde_json::from_str(text.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?).ok()?;
        decision.is_valid(session_id).then_some(decision)
    }

    fn snapshot(&self) -> std::io::Result<ResponseItem> {
        Ok(ResponseItem::Message {
            id: None,
            role: "developer".into(),
            content: vec![ContentItem::InputText {
                text: format!("{PREFIX}{}{SUFFIX}", self.canonical_text()?),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
    }
}

impl Session {
    /// Called only by the matched request consumer, never from generic tool output.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The plan publication gate orders this receipt with plan publications through durable commit"
    )]
    pub(crate) async fn record_accepted_user_decision(
        self: &Arc<Self>,
        turn: &Arc<TurnContext>,
        call_id: &str,
        request: RequestUserInputArgs,
        response: &RequestUserInputResponse,
        cancellation: CancellationToken,
    ) -> std::io::Result<()> {
        let Some(decision) =
            AcceptedUserDecision::new(self.thread_id, &turn.sub_id, call_id, request, response)
        else {
            return Ok(());
        };
        let text = decision.canonical_text()?;
        let snapshot = decision.snapshot()?;
        let accepted = AcceptedPlanInput::from_texts(&turn.sub_id, [text.as_str()].into_iter());
        let session = Arc::clone(self);
        // Use the same tracked, cancellation-safe commit lifetime as input/history.
        let in_flight = self.retain_tool_dispatch_commit();
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let _commit = session
                .durable_history_commit_gate
                .acquire()
                .await
                .map_err(|_| std::io::Error::other("history commit gate is closed"))?;
            let _publication = session.services.plan_store.publication_guard().await;
            // Before admission, cancellation cannot mint a new witness. After
            // append starts the accepted input commit is owned through completion.
            if cancellation.is_cancelled() {
                return Err(std::io::Error::other(
                    "user decision cancelled before publication",
                ));
            }
            session
                .services
                .plan_store
                .record_accepted_decision(accepted, async {
                    let result = session
                        .persist_rollout_items_durable(&[RolloutItem::ResponseItem(snapshot)])
                        .await;
                    if result.is_err() {
                        // A failed durability barrier may have appended metadata.
                        // Reopen must reconcile it before further live publications.
                        session.durable_history_commit_gate.close();
                    }
                    result
                })
                .await
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

#[cfg(test)]
#[path = "user_decision_tests.rs"]
mod tests;
