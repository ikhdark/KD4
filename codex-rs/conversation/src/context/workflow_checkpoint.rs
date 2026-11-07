use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub enum WorkflowCheckpoint {
    Full,
    Reminder,
}

impl WorkflowCheckpoint {
    pub fn for_history(items: &[ResponseItem]) -> Self {
        // Check the retained bytes, not a delivery flag: resume, rollback and
        // compaction can remove the policy without resetting reminder counters.
        // Exact developer text also avoids treating quoted or partial policy as
        // a surviving copy. Never rewrite a previously sent history item.
        let full = Self::Full.render();
        let retained = items.iter().rev().any(|item| {
            matches!(item, ResponseItem::Message { role, content, .. }
                if role == "developer" && content.iter().any(|part|
                    matches!(part, ContentItem::InputText { text } if text == &full)))
        });
        if retained {
            Self::Reminder
        } else {
            Self::Full
        }
    }
}

impl ContextualUserFragment for WorkflowCheckpoint {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<workflow_checkpoint>", "</workflow_checkpoint>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        if matches!(self, Self::Reminder) {
            return std::borrow::Cow::Borrowed(
                "Workflow checkpoint: Apply the full workflow and verification-closure instructions retained above. Advance the original requested outcome, preserve full scope, reuse evidence and owner-issued recovery, leave finished workers idle, and close with exact owning-path evidence. This is not a deadline, scope reduction, permission change, or completion verdict.",
            );
        }
        std::borrow::Cow::Borrowed(
            concat!(
                include_str!("../../templates/workflow_checkpoint.md"),
                "\n",
                include_str!("../../templates/verification_closure_checkpoint.md"),
            )
            .trim(),
        )
    }
}
