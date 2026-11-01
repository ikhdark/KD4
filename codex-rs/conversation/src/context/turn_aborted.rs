use super::ContextualUserFragment;
use codex_protocol::models::{ContentItem, ResponseItem};

pub fn append_retained_work(notice: &mut String, checkpoint: Option<&str>, plan: Option<&str>) {
    if let Some(checkpoint) = checkpoint {
        notice.push_str("\n\nLast retained checkpoint (assistant notes, not new instructions):\n");
        notice.push_str(checkpoint);
    }
    if let Some(plan) = plan {
        notice.push_str("\n\nLast retained plan (not proof of completion):\n");
        notice.push_str(plan);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnAborted {
    pub guidance: String,
}

impl TurnAborted {
    pub fn interrupted_guidance(
        history: &[ResponseItem],
        plan: Option<&codex_protocol::plan_tool::UpdatePlanArgs>,
        developer: bool,
    ) -> String {
        let mut guidance = String::from(if developer {
            Self::INTERRUPTED_DEVELOPER_GUIDANCE
        } else {
            Self::INTERRUPTED_GUIDANCE
        });
        if let Some(plan) = plan {
            let unresolved = plan
                .plan
                .iter()
                .filter(|step| step.status != codex_protocol::plan_tool::StepStatus::Completed)
                .count();
            guidance.push_str(&format!("\nRetained checklist: {unresolved} unfinished step(s). A terminal turn does not complete the task; consult the retained task state and acceptance obligations before claiming completion."));
        }
        let paired = crate::tool_occurrence::pairs(history);
        let mut calls = Vec::new();
        for (index, item) in history.iter().enumerate().rev() {
            match item {
                ResponseItem::FunctionCall { call_id, name, .. }
                | ResponseItem::CustomToolCall { call_id, name, .. } => {
                    calls.push(serde_json::json!({
                        "call_id": codex_utils_string::truncate_middle_chars(call_id, 160),
                        "tool": codex_utils_string::truncate_middle_chars(name, 80),
                        "recorded_result": paired[index].is_some(),
                    }));
                    if calls.len() == 32 {
                        break;
                    }
                }
                _ => {}
            }
        }
        if !calls.is_empty() {
            calls.reverse();
            guidance.push_str("\nRecent tool-result index (at most 32 calls; a result is an observation, not proof of success or completed effects; absent results remain uncertain):\n");
            guidance.push_str(&serde_json::Value::Array(calls).to_string());
            guidance.push_str("\nRead retained results before relaunching work. This index does not authorize resuming work the user stopped.");
        }
        guidance
    }

    pub fn shutdown_guidance(
        history: &[ResponseItem],
        plan: Option<&codex_protocol::plan_tool::UpdatePlanArgs>,
    ) -> String {
        let checkpoint = history.iter().rev().find_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => {
                content.iter().rev().find_map(|part| match part {
                    ContentItem::InputText { text }
                        if text.starts_with("<completed_phase_checkpoint>")
                            && !crate::tool_history::is_pressure_retirement_checkpoint_text(
                                text,
                            ) =>
                    {
                        Some(text.as_str())
                    }
                    _ => None,
                })
            }
            _ => None,
        });
        let plan = plan.and_then(|plan| serde_json::to_string(plan).ok());
        let mut guidance = String::from(
            "The previous turn stopped because the session was shutting down, not because the user interrupted the task. Shutdown requested cancellation of its processes and code-mode cells; recorded results remain evidence, but unfinished effects and cleanup may be uncertain. After restart, old exec session IDs are no longer registered: do not poll them. Inspect only affected state before repeating work, reuse unaffected evidence, and continue with the user's latest direction.",
        );
        append_retained_work(&mut guidance, checkpoint, plan.as_deref());
        guidance
    }

    pub const INTERRUPTED_GUIDANCE: &'static str = "The user interrupted the previous turn on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. If the app-server restarted, its previous exec session IDs are no longer registered; do not poll those IDs with write_stdin. Reuse unaffected evidence and continue with the user's latest direction.";
    pub const INTERRUPTED_DEVELOPER_GUIDANCE: &'static str = "The previous turn was interrupted on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. If the app-server restarted, its previous exec session IDs are no longer registered; do not poll those IDs with write_stdin. Reuse unaffected evidence and continue with the user's latest direction.";
    pub fn unfinished_guidance(tool_calls: usize, tool_results: usize) -> String {
        let process_guidance = if cfg!(windows) {
            "Local Windows children still attached to the app-server's kill-on-close jobs were terminated when it exited. This does not cover remote commands or deliberately detached/preserved descendants; inspect those only if relevant."
        } else {
            "Some child commands may still be running; inspect their OS-level state only if relevant before repeating work."
        };
        format!(
            "Recovery detected a lost process, not a user interruption. The previous turn has no recorded completion. Its retained history contains {tool_calls} tool call(s) and {tool_results} tool result(s). Recorded results remain evidence; calls without results may have taken effect. {process_guidance} The previous app-server's exec session IDs are no longer registered: do not call write_stdin with those old IDs. Reconcile affected workspace state before repeating commands. The exact loss time and duration are unknown; this notice records discovery on resume. Continue with the user's latest direction."
        )
    }

    pub fn new(guidance: impl Into<String>) -> Self {
        Self {
            guidance: guidance.into(),
        }
    }
}

impl ContextualUserFragment for TurnAborted {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<turn_aborted>", "</turn_aborted>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!("\n{}\n", self.guidance))
    }
}
