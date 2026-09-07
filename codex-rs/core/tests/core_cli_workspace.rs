#![allow(clippy::expect_used)]

// Bounded replacement shard for the legacy `all` integration target.
// CLI, workspace-context, and shell-invocation integration tests.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.
pub use codex_protocol::error;

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "agents_md.rs"]
    mod agents_md;
    #[path = "cli_stream.rs"]
    mod cli_stream;
    #[path = "deprecation_notice.rs"]
    mod deprecation_notice;
    #[path = "live_cli.rs"]
    mod live_cli;
    #[path = "remote_env.rs"]
    mod remote_env;
    #[path = "user_shell_cmd.rs"]
    mod user_shell_cmd;
}
