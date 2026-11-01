//! Persisted workspace observations, never an authority or freshness proof.
use serde::{Serialize, Deserialize};
use std::path::PathBuf;
pub use codex_command_analysis::git_dirs::resolve_git_dirs;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceEvidenceIdentity {
    // These fingerprints combine independent observations. Equality is useful
    // for invalidation, but never proves current bytes or one coherent snapshot
    // against arbitrary external writers. Consumers must retain that scope.
    /// Repository discovery or capture failed. Unlike a proven non-Git `None`, this
    /// value never authorizes reuse, even when two failed captures are equal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unavailable: bool,
    #[serde(default)]
    pub repository_root: Option<String>,
    pub head_identity: Option<String>,
    pub index_identity: Option<String>,
    pub worktree_identity: Option<String>,
}

impl PartialEq for WorkspaceEvidenceIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.unavailable == other.unavailable
            && self.repository_root == other.repository_root
            && self.head_identity == other.head_identity
            && self.index_identity == other.index_identity
            && self.worktree_identity == other.worktree_identity
    }
}

impl Eq for WorkspaceEvidenceIdentity {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourcePathChangeObservation {
    watcher_epoch: u64,
    // Numeric epochs can repeat after PID reuse. Legacy observations remain
    // recoverable, but cannot prove continuity with a newly created watcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watcher_incarnation: Option<uuid::Uuid>,
    watcher_generation: u64,
    #[serde(default)]
    registration_generation: u64,
    repo_root: PathBuf,
    path: PathBuf,
    #[serde(default)]
    recursive: bool,
    // The pathname used by the caller is a dependency too: replacing a link
    // changes the read without touching its previously resolved target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<Box<SourcePathChangeObservation>>,
}

impl SourcePathChangeObservation {
    pub fn source_dependency(&self) -> crate::tool_history::SourceDependencyV1 {
        let path = self.alias.as_ref().map_or(&self.path, |alias| &alias.path);
        crate::tool_history::SourceDependencyV1::new(path, self.recursive)
    }
}
