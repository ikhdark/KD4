//! Typed, durable coordination state for MultiAgentV2 assignments.
//!
//! The store owns a private SQLite database and mutation snapshots below the
//! configured Codex home. It depends on [`codex_state::StateRuntime`] only to
//! locate that home, keeping `codex-state` independent of coordination types.

mod error;
mod id;
mod local;
mod model;
mod scope;
mod store;

pub use error::StoreError;
pub use error::StoreResult;
pub use id::AssignmentId;
pub use id::AttemptId;
pub use id::MutationEventId;
pub use id::WakeEventId;
pub use local::LocalAgentTaskStore;
pub use model::*;
pub use scope::RepoScope;
pub use scope::normalize_repo_scopes;
pub use store::AgentTaskStore;
pub use store::TaskStoreFuture;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
