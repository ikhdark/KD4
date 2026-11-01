//! Immutable instruction observation metadata. Acquisition remains in core.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Freshness of the AGENTS.md observation attached to one sampling request.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentsMdFreshness {
    Refreshed,
    IncompleteRead,
    #[default]
    CachedFallback,
}

impl AgentsMdFreshness {
    pub const fn model_visible_description(self) -> &'static str {
        match self {
            Self::Refreshed => {
                "Result provenance: active_instruction_snapshot; freshness: global_snapshot_retained_project_files_refreshed_for_this_sampling_step."
            }
            Self::IncompleteRead => {
                "Result provenance: direct_file_read; freshness: incomplete_read_may_omit_instructions."
            }
            Self::CachedFallback => {
                "Result provenance: cached_observation; freshness: cached_may_be_stale."
            }
        }
    }

    /// Strip only known persisted provenance prefixes, including the former
    /// session-snapshot claim. Older rollouts must not turn that metadata into rules.
    pub fn strip_model_visible_description(self, text: &str) -> Option<&str> {
        text.strip_prefix(self.model_visible_description())
            .or_else(|| {
                (self == Self::Refreshed)
                    .then(|| text.strip_prefix("Result provenance: active_instruction_snapshot; freshness: complete_session_snapshot_for_current_scope; project_files_not_rechecked_each_sampling_step."))
                    .flatten()
            })
            .and_then(|text| text.strip_prefix("\n\n"))
    }
}

#[derive(Clone)]
pub struct RepositoryStableContextBundle {
    pub identity: [u8; 32],
    pub rendered: Arc<str>,
    pub reused: bool,
    pub semantic_replacement: bool,
    pub source_snapshot: Option<Arc<serde_json::Value>>,
}

impl RepositoryStableContextBundle {
    pub fn metadata(&self) -> ([u8; 32], bool, bool) {
        (self.identity, self.reused, self.semantic_replacement)
    }

    pub fn as_cached(&self) -> Self {
        Self {
            identity: self.identity,
            rendered: Arc::clone(&self.rendered),
            reused: true,
            semantic_replacement: false,
            source_snapshot: self.source_snapshot.clone(),
        }
    }
}
