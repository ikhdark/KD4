#![allow(clippy::expect_used)]

// Bounded codex-core integration shard.
// Command execution, sandboxing, approval, permission, and hook integration
// tests. The Windows sandbox modules (`approvals`, `request_permissions`, and
// `windows_sandbox`) stage the sandbox helpers and share one cross-process lock.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.

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
    #[path = "hooks_windows.rs"]
    mod hooks_windows;
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
    #[path = "windows_sandbox.rs"]
    mod windows_sandbox;
}
