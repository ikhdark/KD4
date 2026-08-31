#![allow(clippy::expect_used)]

// Bounded replacement shard for the legacy `all` integration target.
// Subagent execution, delegation, collaboration, and review integration tests.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.
pub use codex_protocol::error;

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "agent_execution.rs"]
    mod agent_execution;
    #[path = "agent_jobs.rs"]
    mod agent_jobs;
    #[path = "agent_websocket.rs"]
    mod agent_websocket;
    #[path = "auto_review.rs"]
    mod auto_review;
    #[path = "codex_delegate.rs"]
    mod codex_delegate;
    #[path = "collaboration_instructions.rs"]
    mod collaboration_instructions;
    #[path = "investigation_evidence_schema.rs"]
    mod investigation_evidence_schema;
    #[path = "multi_agent_mode.rs"]
    mod multi_agent_mode;
    #[path = "request_user_input.rs"]
    mod request_user_input;
    #[path = "review.rs"]
    mod review;
    #[path = "subagent_notifications.rs"]
    mod subagent_notifications;
}
