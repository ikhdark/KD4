use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;

use super::session::Session;
use super::turn_context::TurnContext;
use crate::context::ContextualUserFragment;
use crate::context_manager::is_user_turn_boundary;

#[derive(Default)]
pub(crate) struct WorkflowCheckpointState {
    tool_outputs_since_user: u64,
    last_workflow_checkpoint: u64,
}

impl WorkflowCheckpointState {
    pub(super) fn note_recorded_items(&mut self, items: &[ResponseItem]) {
        for item in items {
            let user_boundary = is_user_turn_boundary(item);
            let tool_output = matches!(
                item,
                ResponseItem::FunctionCallOutput { .. }
                    | ResponseItem::CustomToolCallOutput { .. }
                    | ResponseItem::ToolSearchOutput { .. }
            );
            if user_boundary {
                self.tool_outputs_since_user = 0;
                self.last_workflow_checkpoint = 0;
            }
            if tool_output {
                self.tool_outputs_since_user = self.tool_outputs_since_user.saturating_add(1);
            }
        }
    }

    fn take_workflow_checkpoint_due(&mut self) -> bool {
        // A sparse advisory, never a generation budget or an assertion that
        // inspection made no progress. Doubling bounds repeated prompt cost.
        let due = self.tool_outputs_since_user >= 16
            && (self.last_workflow_checkpoint == 0
                || self.tool_outputs_since_user / 2 >= self.last_workflow_checkpoint);
        if due {
            self.last_workflow_checkpoint = self.tool_outputs_since_user;
        }
        due
    }
}

pub(super) async fn maybe_record_workflow_checkpoint(
    sess: &Session,
    turn_context: &TurnContext,
    base_policy: &str,
    file_edits: bool,
    guidance_edits: bool,
) -> CodexResult<()> {
    // Explicit authored policies retain their replacement semantics. Never load
    // canonical rules over an effective base that does not opt into policy IDs.
    if !base_policy.contains("<!-- policy:") {
        return Ok(());
    }
    let workflow_checkpoint = {
        let mut state = sess.state.lock().await;
        state
            .workflow_checkpoint
            .take_workflow_checkpoint_due()
            .then(|| {
                crate::context::WorkflowCheckpoint::for_history(
                    state.history.raw_items(),
                    base_policy,
                    file_edits,
                    guidance_edits,
                )
            })
    };
    if let Some(checkpoint) = workflow_checkpoint {
        let item = ContextualUserFragment::into(checkpoint);
        sess.record_conversation_items(turn_context, std::slice::from_ref(&item))
            .await?;
    }
    Ok(())
}

/// Diagnose duplicate machine-readable definitions before the first sampling
/// request. Never interpret prose or change the effective base instructions.
pub(super) fn audit_effective_policy_preflight(text: &str) {
    if !text.contains("<!-- policy:") {
        return;
    }
    let mut definitions = std::collections::BTreeMap::new();
    let mut open = None;
    let mut offset = 0;
    let mut fence = None;
    for raw in text.split_inclusive('\n') {
        let line_start = offset;
        offset += raw.len();
        let line = raw.trim_end();
        let after_indent = line.trim_start_matches(' ');
        if line.len() - after_indent.len() <= 3
            && let Some(marker @ (b'`' | b'~')) = after_indent.bytes().next()
        {
            let count = after_indent.bytes().take_while(|&b| b == marker).count();
            if count >= 3 {
                match fence {
                    None => fence = Some((marker, count)),
                    Some((opened, width))
                        if opened == marker
                            && count >= width
                            && after_indent[count..].is_empty() =>
                    {
                        fence = None
                    }
                    _ => {}
                }
                continue;
            }
        }
        if fence.is_some() {
            continue;
        }
        if let Some(id) = line
            .strip_prefix("<!-- policy:")
            .and_then(|s| s.strip_suffix(" -->"))
        {
            open = Some((id, offset));
        } else if let Some(id) = line
            .strip_prefix("<!-- /policy:")
            .and_then(|s| s.strip_suffix(" -->"))
            && let Some((opened, body_start)) = open.take()
            && opened == id
            && id.len() <= 80
            && !id.is_empty()
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".:_-".contains(&c))
        {
            let body = text[body_start..line_start].trim();
            if !body.is_empty() {
                use std::collections::btree_map::Entry;
                match definitions.entry(id) {
                    Entry::Vacant(entry) => {
                        entry.insert((body, false, false));
                    }
                    Entry::Occupied(mut entry) => {
                        let (prior, redundant, conflicting) = entry.get_mut();
                        if *prior == body && !*redundant {
                            tracing::info!(
                                policy_id = id,
                                "instruction preflight: redundant policy id"
                            );
                            *redundant = true;
                        } else if *prior != body && !*conflicting {
                            tracing::warn!(
                                policy_id = id,
                                "instruction preflight: conflicting policy id"
                            );
                            *conflicting = true;
                        }
                    }
                }
            }
        }
    }
}
