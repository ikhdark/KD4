#![allow(clippy::expect_used)]

// Bounded replacement shard for the legacy `all` integration target.
// Windows-only hook and sandbox integration tests.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.
pub use codex_protocol::error;

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "hooks_windows.rs"]
    mod hooks_windows;
    #[path = "windows_sandbox.rs"]
    mod windows_sandbox;
}
