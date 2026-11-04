//! Turn-local answer acceptance, revision invalidation and visible-output identity.
//!
//! The parent turn loop owns repair admission, hooks and final publication.

use crate::stream_events_utils::FinalizedAssistantOutput;
use codex_protocol::models::MessagePhase;

/// Host-only, turn-scoped opt-in; schema absence and model text cannot grant it.
pub(in crate::session) struct NoProseAuthorization;

pub(super) const MISSING_ANSWER_REPAIR_DIRECTIVE: &str = "The response ended without a completed final answer. Deliver the answer required by the current request, including any requested explanation or truthful blocker; Commentary and successful tool stdout are not a final answer. Do not repeat completed effects or invent validation. If and only if the request explicitly authorizes no prose, and no structured output schema applies, declare that disposition with an empty final_answer message rather than an empty response. Otherwise supply a nonempty final answer. This is the single missing-answer repair attempt.";

#[derive(Debug, Default)]
pub(super) enum AnswerDelivery {
    #[default]
    Missing,
    Message(codex_protocol::protocol::AcceptedFinalMessage),
    /// An explicit completed final item, not a reasoning-only/empty response.
    NoProse,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum VisibleOutputNovelty {
    Unobserved,
    NewCommunication,
    ExactRepeat,
    Replay,
}

/// Turn-local only: one answer and one visible identity. Observation is not
/// permission to suppress output, skip reasoning, or accept unfinished work.
#[derive(Default)]
pub(super) struct TurnOutputState {
    pub(super) revision: Option<String>,
    pub(super) answer: AnswerDelivery,
    pub(super) last_visible: Option<(String, Option<MessagePhase>, [u8; 32])>,
    pub(super) missing_answer_repair_attempted: bool,
    pub(super) no_prose_authorized: bool,
}

impl TurnOutputState {
    pub(super) fn reconcile_revision(&mut self, revision: &str) {
        if self.revision.as_deref() != Some(revision) {
            self.invalidate_answer();
            self.last_visible = None;
            self.revision = Some(revision.to_string());
        }
    }

    fn invalidate_answer(&mut self) {
        self.answer = AnswerDelivery::Missing;
    }

    pub(super) fn invalidate_for_repair(&mut self) {
        self.invalidate_answer();
        self.last_visible = None;
        self.revision = None;
    }

    pub(super) fn observe(
        &mut self,
        output: Option<FinalizedAssistantOutput>,
        candidate: Option<codex_protocol::protocol::AcceptedFinalMessage>,
        revision: String,
        allow_answer: bool,
        allow_no_prose: bool,
    ) -> VisibleOutputNovelty {
        use sha2::Digest;
        self.reconcile_revision(&revision);
        if !allow_answer {
            // Output accompanying tools may precede their effects/results.
            self.invalidate_answer();
        } else if let Some(candidate) = candidate {
            self.answer = AnswerDelivery::Message(candidate);
        } else if allow_no_prose
            && (self.no_prose_authorized || self.missing_answer_repair_attempted)
            && output.as_ref().is_some_and(|output| {
                output.phase == Some(MessagePhase::FinalAnswer) && output.text.is_empty()
            })
        {
            self.answer = AnswerDelivery::NoProse;
        }
        let Some(output) = output else {
            // Necessary silent inference has no observable novelty verdict.
            return VisibleOutputNovelty::Unobserved;
        };
        let digest: [u8; 32] = sha2::Sha256::digest(output.text.as_bytes()).into();
        let novelty = match &self.last_visible {
            Some((id, phase, previous)) if phase == &output.phase && previous == &digest => {
                if id == &output.item_id {
                    VisibleOutputNovelty::Replay
                } else {
                    VisibleOutputNovelty::ExactRepeat
                }
            }
            _ => VisibleOutputNovelty::NewCommunication,
        };
        self.last_visible = Some((output.item_id, output.phase, digest));
        tracing::debug!(
            target: "codex_core::session::turn",
            ?novelty,
            "task-bound visible output observation; semantic necessity unknown"
        );
        novelty
    }

    pub(super) fn has_answer(&self) -> bool {
        !matches!(self.answer, AnswerDelivery::Missing)
    }

    pub(super) fn answer_text(&self) -> Option<String> {
        match &self.answer {
            AnswerDelivery::Message(message) => Some(message.text.clone()),
            _ => None,
        }
    }

    pub(super) fn take_answer(&mut self) -> Option<codex_protocol::protocol::AcceptedFinalMessage> {
        match std::mem::take(&mut self.answer) {
            AnswerDelivery::Message(message) => Some(message),
            _ => None,
        }
    }
}

pub(super) fn final_message_candidate_from_output(
    output: crate::stream_events_utils::FinalizedAssistantOutput,
) -> Option<codex_protocol::protocol::AcceptedFinalMessage> {
    use codex_protocol::models::MessagePhase;
    use codex_protocol::protocol::AcceptedFinalMessageOrigin;
    if matches!(output.phase, Some(MessagePhase::Commentary | MessagePhase::PartialAnswer))
        || output.text.trim().is_empty()
    {
        return None;
    }
    Some(codex_protocol::protocol::AcceptedFinalMessage {
        item_id: output.item_id,
        text: output.text,
        origin: if output.phase == Some(MessagePhase::FinalAnswer) {
            AcceptedFinalMessageOrigin::AssistantFinal
        } else {
            AcceptedFinalMessageOrigin::LegacyUnphased
        },
        completion_attempt_id: String::new(),
    })
}

pub(super) fn update_final_message_candidate(
    current: &mut Option<codex_protocol::protocol::AcceptedFinalMessage>,
    candidate: codex_protocol::protocol::AcceptedFinalMessage,
) {
    use codex_protocol::protocol::AcceptedFinalMessageOrigin;
    let rank = |origin| match origin {
        AcceptedFinalMessageOrigin::AssistantFinal => 2,
        AcceptedFinalMessageOrigin::LegacyUnphased => 1,
        AcceptedFinalMessageOrigin::Plan => 0,
    };
    if current
        .as_ref()
        .is_none_or(|previous| rank(candidate.origin) >= rank(previous.origin))
    {
        *current = Some(candidate);
    }
}
