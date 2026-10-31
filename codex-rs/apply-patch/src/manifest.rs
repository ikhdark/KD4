//! Operation-bound endpoint outcomes. Never reconstructed from Git or later reads.
use crate::AppliedPatchWriteReceipt;
use crate::ApplyPatchError;
use crate::Hunk;
use codex_exec_server::FileMutationOutcome;
use codex_utils_path_uri::PathUri;
use serde::Serialize;
use sha2::Digest;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PatchByteIdentity {
    Unknown,
    Absent,
    Present {
        sha256: String,
        bytes: Option<usize>,
    },
}

impl PatchByteIdentity {
    fn from_receipt(receipt: Option<&AppliedPatchWriteReceipt>) -> Self {
        receipt.map_or(Self::Absent, |receipt| Self::Present {
            sha256: receipt.sha256.clone(),
            bytes: Some(receipt.bytes),
        })
    }

    fn from_content(content: Option<&str>) -> Self {
        content.map_or(Self::Absent, |content| Self::Present {
            sha256: hex::encode(sha2::Sha256::digest(content.as_bytes())),
            bytes: Some(content.len()),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchFileOutcome {
    Committed,
    Unchanged,
    Failed,
    Unattempted,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PatchManifestEntry {
    pub path: String,
    pub operation: &'static str,
    pub hunk_ordinal: usize,
    pub hunk_ordinals: Vec<usize>,
    pub operations: Vec<&'static str>,
    /// Moves have separate source and destination receipts, including partial moves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    pub before: PatchByteIdentity,
    pub after: PatchByteIdentity,
    pub outcome: PatchFileOutcome,
    pub rolled_back: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PatchManifest {
    pub entries: Vec<PatchManifestEntry>,
    active_hunk: Option<usize>,
    pub rolling_back: bool,
    pub final_identities: BTreeMap<String, PatchByteIdentity>,
}

impl PatchManifest {
    pub fn new(hunks: &[Hunk], cwd: &PathUri) -> Self {
        let mut entries = Vec::new();
        for (index, hunk) in hunks.iter().enumerate() {
            let source = hunk.resolve_path(cwd).map_or_else(
                |_| hunk.path().display().to_string(),
                |path| path.to_string(),
            );
            let (operation, destination) = match hunk {
                Hunk::AddFile { .. } => ("add", None),
                Hunk::DeleteFile { .. } => ("delete", None),
                Hunk::UpdateFile {
                    move_path: Some(path),
                    ..
                } => ("move", Some(path)),
                Hunk::UpdateFile { .. } => ("update", None),
            };
            let entry = |path, role| PatchManifestEntry {
                path,
                operation,
                role,
                hunk_ordinal: index + 1,
                hunk_ordinals: vec![index + 1],
                operations: vec![operation],
                before: PatchByteIdentity::Unknown,
                after: PatchByteIdentity::Unknown,
                outcome: PatchFileOutcome::Unattempted,
                rolled_back: false,
            };
            // Publication order, not the order of paths in a map.
            if let Some(destination) = destination {
                let path = cwd.join(&destination.to_string_lossy()).map_or_else(
                    |_| destination.display().to_string(),
                    |path| path.to_string(),
                );
                entries.push(entry(path, Some("destination")));
            }
            entries.push(entry(source, destination.map(|_| "source")));
        }
        Self {
            entries,
            ..Self::default()
        }
    }

    pub fn begin_hunk(&mut self, index: usize) {
        self.active_hunk = Some(index + 1);
    }
    pub fn finish_hunk(&mut self) {
        self.active_hunk = None;
    }

    pub fn snapshot(&self) -> Vec<PatchManifestEntry> {
        let affected_or_uncertain = self.entries.iter().filter(|entry|
            matches!(entry.outcome, PatchFileOutcome::Committed | PatchFileOutcome::Uncertain))
            .map(|entry| entry.path.as_str()).collect::<std::collections::BTreeSet<_>>();
        let mut files = Vec::<PatchManifestEntry>::new();
        let mut indices = BTreeMap::<&str, usize>::new();
        for entry in &self.entries {
            if let Some(&index) = indices.get(entry.path.as_str()) {
                let file = &mut files[index];
                if file.outcome == PatchFileOutcome::Unattempted {
                    file.before = entry.before.clone();
                }
                if !file.hunk_ordinals.contains(&entry.hunk_ordinal) {
                    file.hunk_ordinals.push(entry.hunk_ordinal);
                }
                if !file.operations.contains(&entry.operation) {
                    file.operations.push(entry.operation);
                }
                file.operation = entry.operation;
                file.role = entry.role;
                file.rolled_back |= entry.rolled_back;
                // Keep failures/uncertainty until a later actual receipt supplies
                // a final identity; an unreached suffix supplies no identity.
                if entry.outcome != PatchFileOutcome::Unattempted {
                    file.after = entry.after.clone();
                    file.outcome = entry.outcome;
                }
            } else {
                indices.insert(&entry.path, files.len());
                files.push(entry.clone());
            }
        }
        for file in &mut files {
            if let Some(after) = self.final_identities.get(&file.path) {
                file.after = after.clone();
                file.outcome = if *after == PatchByteIdentity::Unknown {
                    if affected_or_uncertain.contains(file.path.as_str()) {
                        PatchFileOutcome::Uncertain
                    } else { PatchFileOutcome::Failed }
                } else if file.before != file.after {
                    PatchFileOutcome::Committed
                } else if file.outcome == PatchFileOutcome::Failed {
                    PatchFileOutcome::Failed
                } else {
                    PatchFileOutcome::Unchanged
                };
            }
            file.rolled_back &=
                file.before == file.after && file.after != PatchByteIdentity::Unknown;
        }
        files
    }

    pub fn reject(&mut self, error: &ApplyPatchError) {
        let mismatches = match error {
            ApplyPatchError::PatchContextMismatch(mismatch) => std::slice::from_ref(mismatch),
            ApplyPatchError::MultiplePatchConflicts { mismatches, .. } => mismatches,
            _ => &[],
        };
        for mismatch in mismatches {
            for entry in &mut self.entries {
                if entry.hunk_ordinal == mismatch.hunk_ordinal
                    && entry.path == mismatch.canonical_path
                {
                    entry.outcome = PatchFileOutcome::Failed;
                    // Chained preflight hashes describe in-memory results, not file bytes.
                    if mismatch.edits_result_of_hunk.is_none() {
                        entry.before = PatchByteIdentity::Present {
                            sha256: mismatch.current_content_sha256.clone(),
                            bytes: None,
                        };
                        entry.after = entry.before.clone();
                    }
                }
            }
        }
        if let Some(hunk) = self.active_hunk
            && !self.entries.iter().any(|entry| {
                entry.hunk_ordinal == hunk
                    && matches!(
                        entry.outcome,
                        PatchFileOutcome::Failed | PatchFileOutcome::Uncertain
                    )
            })
            && let Some(entry) = self.entries.iter_mut().find(|entry| {
                entry.hunk_ordinal == hunk && entry.outcome == PatchFileOutcome::Unattempted
            })
        {
            entry.outcome = PatchFileOutcome::Failed;
        }
    }

    fn target(&mut self, path: &PathUri) -> Option<&mut PatchManifestEntry> {
        let path = path.to_string();
        let rolling_back = self.rolling_back;
        let active_hunk = self.active_hunk;
        self.entries.iter_mut().rev().find(|entry| {
            entry.path == path
                && if rolling_back {
                    !entry.rolled_back && entry.outcome == PatchFileOutcome::Committed
                } else {
                    Some(entry.hunk_ordinal) == active_hunk
                }
        })
    }

    pub fn uncertain(&mut self, path: &PathUri) {
        if let Some(entry) = self.target(path) {
            entry.outcome = PatchFileOutcome::Uncertain;
            entry.after = PatchByteIdentity::Unknown;
        }
        self.final_identities
            .insert(path.to_string(), PatchByteIdentity::Unknown);
    }

    /// Reuse the mutation owner's existing failure reconciliation, not a later
    /// Git inventory. A failed rollback can leave a known committed prefix.
    pub fn confirm_no_write(&mut self, path: &PathUri, expected: Option<&str>) {
        let path = path.to_string();
        let rolling_back = self.rolling_back;
        let active_hunk = self.active_hunk;
        if let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            entry.path == path
                && if rolling_back {
                    !entry.rolled_back && entry.outcome == PatchFileOutcome::Uncertain
                } else {
                    Some(entry.hunk_ordinal) == active_hunk
                }
        }) {
            entry.after = PatchByteIdentity::from_content(expected);
            if rolling_back {
                entry.outcome = PatchFileOutcome::Committed;
            } else {
                entry.before = entry.after.clone();
                entry.outcome = PatchFileOutcome::Failed;
            }
        }
        self.final_identities
            .insert(path, PatchByteIdentity::from_content(expected));
    }

    pub fn record(
        &mut self,
        path: &PathUri,
        expected: Option<&str>,
        replacement: Option<&AppliedPatchWriteReceipt>,
        outcome: &FileMutationOutcome,
    ) {
        let rolling_back = self.rolling_back;
        let Some(entry) = self.target(path) else {
            return;
        };
        match outcome {
            FileMutationOutcome::Committed | FileMutationOutcome::Unchanged => {
                // Only a successful conditional receipt validates the expected bytes.
                if !rolling_back {
                    entry.before = PatchByteIdentity::from_content(expected);
                }
                entry.after = PatchByteIdentity::from_receipt(replacement);
                entry.outcome = if (rolling_back && entry.before == entry.after)
                    || matches!(outcome, FileMutationOutcome::Unchanged)
                {
                    PatchFileOutcome::Unchanged
                } else {
                    PatchFileOutcome::Committed
                };
                entry.rolled_back |= rolling_back;
            }
            FileMutationOutcome::Conflict => {
                entry.outcome = if rolling_back {
                    PatchFileOutcome::Uncertain
                } else {
                    PatchFileOutcome::Failed
                };
                entry.after = PatchByteIdentity::Unknown;
            }
            FileMutationOutcome::RecoveryRequired { .. } => {
                entry.outcome = PatchFileOutcome::Uncertain;
                entry.after = PatchByteIdentity::Unknown;
            }
        }
        let after = entry.after.clone();
        self.final_identities.insert(path.to_string(), after);
    }
}
