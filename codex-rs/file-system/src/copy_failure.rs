//! Bounded effect context for a non-transactional copy failure.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CopyFailure {
    pub completed_entries: u64,
    pub target_may_have_changed: bool,
    pub destination: String,
    pub cause: String,
}

impl std::fmt::Display for CopyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; copy failed after {} completed destination entries; current target may have changed: {}. Inspect {} before retrying; no rollback was attempted.",
            self.cause, self.completed_entries, self.target_may_have_changed, self.destination
        )
    }
}

impl std::error::Error for CopyFailure {}

impl CopyFailure {
    pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}
