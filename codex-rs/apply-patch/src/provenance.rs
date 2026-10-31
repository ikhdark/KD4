//! Bounded process-local history of committed edits, not ownership authority.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{AppliedPatchDelta, AppliedPatchFileChange};

const MAX_ENDPOINTS: usize = 2048;
static HISTORY: LazyLock<Mutex<History>> = LazyLock::new(|| Mutex::new(History::default()));

#[derive(Default)]
struct History {
    sequence: u64,
    unknown_since: u64,
    evicted: bool,
    edits: VecDeque<Edit>,
}

struct Edit {
    sequence: u64,
    key: String,
    path: PathBuf,
    agent: Option<String>,
    operation: Option<String>,
    before: Option<String>,
    after: Option<String>,
    predecessor: Option<u64>,
    source_predecessor: Option<u64>,
    ambiguous: bool,
}

fn key(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    // Endpoint URIs are supplied by the mutation owner. No filesystem reads,
    // path reservation or permissions are acquired by this journal.
    // Do not fold case: Windows can host case-sensitive directories. Unproven
    // aliases may miss a hint, but must never borrow another path's lineage.
    Some(path.to_str()?.replace('\\', "/"))
}

fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

impl History {
    fn push(
        &mut self,
        path: PathBuf,
        before: Option<String>,
        after: Option<String>,
        agent: Option<&str>,
        operation: Option<&str>,
        exact: bool,
    ) {
        let Some(key) = key(&path) else {
            return;
        };
        self.sequence = self.sequence.saturating_add(1);
        let previous = self.edits.iter().rev().find(|edit| edit.key == key);
        let predecessor = previous.map(|edit| edit.sequence);
        let ambiguous = !exact
            || agent.is_none()
            || operation.is_none()
            || previous.is_some_and(|edit| {
                edit.after != before || edit.ambiguous || edit.sequence <= self.unknown_since
            });
        if self.edits.len() == MAX_ENDPOINTS {
            self.edits.pop_front();
            self.evicted = true;
        }
        self.edits.push_back(Edit {
            sequence: self.sequence,
            key,
            path,
            before,
            after,
            agent: agent.map(str::to_owned),
            operation: operation.map(str::to_owned),
            predecessor,
            source_predecessor: None,
            ambiguous,
        });
    }
}

/// Opaque/unrecorded mutation invalidates confidence, never deletes historical
/// lineage. The next query still requires fresh observed bytes from its caller.
pub fn invalidate_change_ownership_hints() {
    let mut history = HISTORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    history.unknown_since = history.sequence;
}

/// Extend the existing committed delta with its producing call and agent. All
/// overlapping endpoint records survive (within the explicit rolling bound),
/// including overwritten rename destinations; last-writer replacement is not
/// used as a substitute for lineage.
pub fn record_change_provenance(
    agent: Option<&str>,
    operation: Option<&str>,
    delta: &AppliedPatchDelta,
) {
    let mut history = HISTORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !delta.is_exact() {
        history.unknown_since = history.sequence;
    }
    for applied in delta.changes() {
        let applied = delta.endpoint_change(applied);
        let after_hash = |text: &str| {
            applied
                .write_receipt
                .as_ref()
                .map_or_else(|| hash(text), |receipt| receipt.sha256.clone())
        };
        match &applied.change {
            AppliedPatchFileChange::Add {
                content,
                overwritten_content,
            } => {
                history.push(
                    applied.path.clone(),
                    overwritten_content.as_deref().map(hash),
                    Some(after_hash(content)),
                    agent,
                    operation,
                    delta.is_exact(),
                );
            }
            AppliedPatchFileChange::Delete { content } => {
                history.push(
                    applied.path.clone(),
                    Some(hash(content)),
                    None,
                    agent,
                    operation,
                    delta.is_exact(),
                );
            }
            AppliedPatchFileChange::Update {
                old_content,
                new_content,
                move_path,
                overwritten_move_content,
            } => {
                if let Some(destination) = move_path {
                    history.push(
                        applied.path.clone(),
                        Some(hash(old_content)),
                        None,
                        agent,
                        operation,
                        delta.is_exact(),
                    );
                    let source_predecessor = history
                        .edits
                        .back()
                        .filter(|edit| edit.path == applied.path)
                        .map(|edit| edit.sequence);
                    history.push(
                        destination.clone(),
                        overwritten_move_content.as_deref().map(hash),
                        Some(after_hash(new_content)),
                        agent,
                        operation,
                        delta.is_exact(),
                    );
                    if let Some(edit) = history
                        .edits
                        .back_mut()
                        .filter(|edit| edit.path == *destination)
                    {
                        edit.source_predecessor = source_predecessor;
                    }
                } else {
                    history.push(
                        applied.path.clone(),
                        Some(hash(old_content)),
                        Some(after_hash(new_content)),
                        agent,
                        operation,
                        delta.is_exact(),
                    );
                }
            }
        }
    }
}

/// `observed` must come from a fresh authorized raw-byte capture. Equality only
/// supports a hint; it cannot detect an unrecorded rewrite to identical bytes,
/// establish exclusive ownership, or authorize an overwrite. Other processes,
/// prior process lifetimes and all unrecorded changes remain unknown.
pub fn change_ownership_hints(observed: &[(PathBuf, Option<String>)]) -> Value {
    let history = HISTORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hints = observed.iter().map(|(path, current)| {
        let key = key(path);
        let latest = history.edits.iter().rev().find(|edit| key.as_ref() == Some(&edit.key));
        let mut selected = history.edits.iter().filter(|edit| key.as_ref() == Some(&edit.key))
            .map(|edit| edit.sequence).collect::<std::collections::BTreeSet<_>>();
        let mut missing_ancestor = false;
        // Reverse traversal follows both overwritten-destination history and
        // rename-source history without dropping another agent's contributions.
        for edit in history.edits.iter().rev() {
            if selected.contains(&edit.sequence) {
                for predecessor in [edit.predecessor, edit.source_predecessor].into_iter().flatten() {
                    missing_ancestor |= !history.edits.iter().any(|entry| entry.sequence == predecessor);
                    selected.insert(predecessor);
                }
            }
        }
        let lineage = history.edits.iter().filter(|edit| selected.contains(&edit.sequence)).collect::<Vec<_>>();
        let reason = match latest {
            None => Some("unrecorded_change"),
            Some(_) if missing_ancestor => Some("lineage_evicted"),
            Some(edit) if edit.ambiguous || edit.sequence <= history.unknown_since => Some("ambiguous_or_unrecorded_mutation"),
            Some(edit) if edit.after != *current => Some("observed_content_does_not_match_recorded_edit"),
            _ => None,
        };
        json!({"path":path, "status":if reason.is_some() { "unknown" } else { "recorded_hint" },
            "unknown_reason":reason, "producing_agent":latest.filter(|_| reason.is_none()).and_then(|edit| edit.agent.as_deref()),
            "producing_operation":latest.filter(|_| reason.is_none()).and_then(|edit| edit.operation.as_deref()),
            "lineage":lineage.iter().map(|edit| json!({"sequence":edit.sequence, "path":edit.path,
                "agent_id":edit.agent, "operation_id":edit.operation, "predecessor":edit.predecessor, "source_predecessor":edit.source_predecessor,
                "before_sha256":edit.before, "after_sha256":edit.after, "ambiguous":edit.ambiguous})).collect::<Vec<_>>()})
    }).collect::<Vec<_>>();
    json!({"scope":"process_local_committed_patch_history", "advisory_only":true,
        "exclusive_ownership":false, "overwrite_authority":false, "coherent_snapshot_verified":false,
        "history_evicted":history.evicted, "unrecorded_changes":"unknown", "hints":hints})
}
