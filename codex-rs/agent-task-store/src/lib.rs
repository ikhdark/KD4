//! Typed, durable coordination state for MultiAgentV2 assignments.
//!
//! The store owns a private SQLite database and task capsules below the
//! configured Codex home. It depends on [`codex_state::StateRuntime`] only to
//! locate that home, keeping `codex-state` independent of coordination types.

mod error;
mod id;
mod local;
mod model;
mod scope;
mod shared;

pub use error::StoreError;
pub use error::StoreResult;
pub use id::AssignmentId;
pub use id::AttemptId;
pub use id::MutationEventId;
pub use id::WakeEventId;
pub use local::DeferredHostCompletionWake;
pub use local::LocalAgentTaskStore;
pub use local::PendingTerminalOutcome;
pub use local::TaskStoreFuture;
pub use model::*;
pub use scope::RepoScope;
pub use scope::normalize_repo_path;
pub use scope::normalize_repo_scopes;
pub use scope::repo_path_identity_keys;
pub use scope::repository_lineage_id;
pub use scope::repository_workspace_id;

