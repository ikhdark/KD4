#![allow(clippy::expect_used)]

// Bounded replacement shard for the legacy `all` integration target.
// Code-mode and MCP client/auth/exposure integration tests.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.
pub use codex_protocol::error;

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "code_mode.rs"]
    mod code_mode;
    #[path = "code_mode_elicitation.rs"]
    mod code_mode_elicitation;
    #[path = "mcp_auth_elicitation.rs"]
    mod mcp_auth_elicitation;
    #[path = "mcp_auth_refresh.rs"]
    mod mcp_auth_refresh;
    #[path = "mcp_refresh_cleanup.rs"]
    mod mcp_refresh_cleanup;
    #[path = "mcp_tool_exposure.rs"]
    mod mcp_tool_exposure;
    #[path = "rmcp_client.rs"]
    mod rmcp_client;
}
