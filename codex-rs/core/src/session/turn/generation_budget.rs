//! Logical-generation admission and spawned-agent context-pressure boundaries.
//!
//! The parent owns sampling and completion; this state distinguishes regular,
//! explicitly terminal and exhausted requests without imposing a generation cap.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_execution::GenerationRequestDisposition;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::WarningEvent;

// Repetition is established by execution receipts and relevant state, never
// by a generation count. Retain only explicit owner/context terminal fences.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LogicalGenerationAdmission {
    Regular,
    Terminal { forced: bool },
    Exhausted,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct LogicalGenerationBudget {
    regular_generations_exhausted: bool,
    terminal_generation_used: bool,
}

impl LogicalGenerationBudget {
    pub(super) fn accepted_user_input(&mut self) {
        self.regular_generations_exhausted = false;
        self.terminal_generation_used = false;
    }

    /// The next regular request becomes the one forced final synthesis.
    pub(super) fn exhaust_regular_generations(&mut self) {
        self.regular_generations_exhausted = true;
    }

    pub(super) fn is_exhausted(self) -> bool {
        self.regular_generations_exhausted && self.terminal_generation_used
    }

    pub(super) fn has_regular_generation_capacity(self) -> bool {
        !self.regular_generations_exhausted
    }

    pub(super) fn admit(&mut self, terminal_requested: bool) -> LogicalGenerationAdmission {
        if terminal_requested {
            if self.terminal_generation_used {
                return LogicalGenerationAdmission::Exhausted;
            }
            self.terminal_generation_used = true;
            return LogicalGenerationAdmission::Terminal { forced: false };
        }
        if !self.regular_generations_exhausted {
            return LogicalGenerationAdmission::Regular;
        }
        if self.terminal_generation_used {
            return LogicalGenerationAdmission::Exhausted;
        }
        self.terminal_generation_used = true;
        LogicalGenerationAdmission::Terminal { forced: true }
    }

    pub(super) fn can_admit(self, terminal_requested: bool) -> bool {
        let mut preview = self;
        preview.admit(terminal_requested) != LogicalGenerationAdmission::Exhausted
    }
}

pub(super) fn generation_budget_blocks_follow_up(
    budget: LogicalGenerationBudget,
    next_generation_request: Option<&GenerationRequestDisposition>,
) -> bool {
    next_generation_request
        .is_some_and(|request| !budget.can_admit(request.terminal_completion_only))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompletionPendingInputDisposition {
    None,
    Continue,
    Defer,
}

pub(super) fn completion_pending_input_disposition(
    budget: LogicalGenerationBudget,
    has_pending_input: bool,
) -> CompletionPendingInputDisposition {
    if !has_pending_input {
        CompletionPendingInputDisposition::None
    } else if budget.has_regular_generation_capacity() {
        CompletionPendingInputDisposition::Continue
    } else {
        CompletionPendingInputDisposition::Defer
    }
}

pub(super) const LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE: &str = "An explicit terminal boundary requires a final tool-free synthesis. Work is suspended, not completed. Do not call tools. Summarize completed work and truthfully report remaining work, failed validation, running processes, and how to resume.";

/// A spawned agent that fills this many context windows in one turn reports
/// what it covered instead of compacting again; its parent splits the rest.
pub(super) const MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS: u32 = 2;
pub(super) const SUBAGENT_CONTEXT_BUDGET_FORCED_TERMINAL_DIRECTIVE: &str = "This spawned agent reached its context budget. Work is suspended, not completed. This is the final tool-free synthesis request. Do not call tools. Report what you covered, and list the unread files or byte ranges and any unfinished work exactly, so the parent can split the remaining scope.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ForcedTerminalReason {
    GenerationBudget,
    SubagentContextBudget,
}

/// Counts a mid-turn context-pressure event. A spawned agent at its budget
/// gets one forced final synthesis instead of another context window.
pub(super) fn observe_context_pressure_event(
    session_source: &SessionSource,
    events: &mut u32,
    budget: &mut LogicalGenerationBudget,
) -> bool {
    *events = events.saturating_add(1);
    let reached = *events >= MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS
        && matches!(
            session_source,
            SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn { .. })
        );
    if reached {
        budget.exhaust_regular_generations();
    }
    reached
}

pub(super) async fn record_forced_terminal_budget_boundary(
    sess: &Session,
    turn_context: &TurnContext,
    reason: ForcedTerminalReason,
) {
    let message = match reason {
        ForcedTerminalReason::GenerationBudget =>
            "This turn reached an explicit terminal boundary. Work is suspended; the assistant will report completed and unfinished work. Send another message to resume.".to_string(),
        ForcedTerminalReason::SubagentContextBudget => format!(
            "This spawned agent reached its budget of {MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS} compactions in one turn. It will report what it covered and the unread scope so its parent can split the remaining work."
        ),
    };
    sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
        .await;
}
