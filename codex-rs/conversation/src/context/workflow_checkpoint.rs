use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use sha2::Digest;

pub struct WorkflowCheckpoint {
    text: String,
}

impl WorkflowCheckpoint {
    pub fn for_history(
        items: &[ResponseItem],
        base_policy: &str,
        file_edits: bool,
        guidance_edits: bool,
    ) -> Self {
        // Check the retained bytes, not a delivery flag: resume, rollback and
        // compaction can remove the policy without resetting reminder counters.
        // Exact developer text also avoids treating quoted or partial policy as
        // a surviving copy. Never rewrite a previously sent history item.
        let mut text = String::new();
        for template in [
            include_str!("../../templates/workflow_checkpoint.md"),
            include_str!("../../templates/verification_closure_checkpoint.md"),
        ] {
            for line in template.lines() {
                if let Some(id) = line.strip_prefix("policy: ") {
                    Self::reference(&mut text, base_policy, id);
                } else if let Some(id) = line.strip_prefix("policy-if-file-edits: ") {
                    if file_edits {
                        text.push_str("Task activity: file edits observed.\n");
                        Self::reference(&mut text, base_policy, id);
                    }
                } else if let Some(id) = line.strip_prefix("policy-if-guidance-edits: ") {
                    if guidance_edits {
                        text.push_str("Task activity: documentation/guidance edits or unknown mutation observed.\n");
                        Self::reference(&mut text, base_policy, id);
                    }
                } else {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
        let checkpoint = Self {
            text: text.trim().to_string(),
        };
        let full = checkpoint.render();
        let retained = items.iter().rev().any(|item| {
            matches!(item, ResponseItem::Message { role, content, .. }
                if role == "developer" && content.iter().any(|part|
                    matches!(part, ContentItem::InputText { text } if text == &full)))
        });
        if retained {
            Self { text: "Workflow checkpoint: Reapply the current authoritative policy references retained above, together with applicable AGENTS.md guidance. This is not a deadline, scope reduction, permission change, or completion verdict.".to_string() }
        } else {
            checkpoint
        }
    }

    fn reference(text: &mut String, base_policy: &str, id: &str) {
        let start = format!("<!-- policy:{id} -->");
        let end = format!("<!-- /policy:{id} -->");
        let body = base_policy
            .split_once(&start)
            .and_then(|(_, tail)| tail.split_once(&end).map(|(body, _)| body.trim()));
        if let Some(body) = body.filter(|body| !body.is_empty())
            && base_policy.matches(&start).count() == 1
            && base_policy.matches(&end).count() == 1
        {
            // A changed policy changes the full-fragment identity. Historical
            // notices remain append-only and never become a stale policy copy.
            let digest = hex::encode(sha2::Sha256::digest(body.as_bytes()));
            text.push_str(&format!("policy: {id} (sha256:{})\n", &digest[..12]));
        } else {
            tracing::warn!(
                policy_id = id,
                "workflow checkpoint policy reference is missing or ambiguous"
            );
            text.push_str(&format!("Unresolved policy reference: {id}; consult the current instructions, not an inferred replacement.\n"));
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
        std::borrow::Cow::Borrowed(&self.text)
    }
}
