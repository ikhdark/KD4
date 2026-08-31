#![allow(clippy::expect_used)]

// Bounded replacement shard for the legacy `all` integration target.
// Command execution, sandboxing, approval, and permission integration tests.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.
pub use codex_protocol::error;

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "apply_patch_cli.rs"]
    mod apply_patch_cli;
    #[path = "approvals.rs"]
    mod approvals;
    #[path = "exec_policy.rs"]
    mod exec_policy;
    #[path = "extension_sandbox.rs"]
    mod extension_sandbox;
    #[path = "permissions_messages.rs"]
    mod permissions_messages;
    #[path = "request_permissions.rs"]
    mod request_permissions;
    #[path = "safety_check_downgrade.rs"]
    mod safety_check_downgrade;
    #[path = "shell_command.rs"]
    mod shell_command;
    #[path = "shell_snapshot.rs"]
    mod shell_snapshot;
    #[path = "unified_exec.rs"]
    mod unified_exec;
    #[path = "unified_exec_process_events.rs"]
    mod unified_exec_process_events;
}
