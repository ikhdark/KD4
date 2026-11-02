//! Conversation state and pure prompt projections, independent of session execution.
#![deny(clippy::print_stdout, clippy::print_stderr)]
pub mod compact;
pub mod context;
pub mod context_manager;
pub mod digest_writer;
pub mod event_mapping;
pub mod instruction_snapshot;
pub mod invariants;
pub mod operation_history;
pub mod plan_store;
pub mod stable_context;
pub mod tool_history;
pub mod tool_occurrence;
pub mod user_decision;
pub mod workspace;
use codex_command_analysis::validation;
pub const RESUME_STATE_LOCATOR: &str = "context:resume-state";
pub const MULTI_AGENT_USAGE_HINT_LOCATOR: &str = "context:multi-agent";
pub const TURN_DIFF_LOCATOR: &str = "context:turn-diff";
