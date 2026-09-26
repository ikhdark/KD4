#![allow(clippy::expect_used)]

// Bounded codex-core integration shard.
// CLI, workspace-context, and shell-invocation integration tests. `cli_stream`
// and `live_cli` spawn the `codex` CLI; the other modules drive in-process
// sessions.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.

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
