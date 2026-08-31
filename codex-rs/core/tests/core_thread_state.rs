#![allow(clippy::expect_used)]

// Bounded replacement shard for the legacy `all` integration target.
// Thread history, compaction, resume/fork, and persisted-state integration tests.
// `compact_resume_fork` and `window_headers` import from `compact`, so the three
// stay in this shard together.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.
pub use codex_protocol::error;

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "compact.rs"]
    mod compact;
    #[path = "compact_remote.rs"]
    mod compact_remote;
    #[path = "compact_resume_fork.rs"]
    mod compact_resume_fork;
    #[path = "fork_thread.rs"]
    mod fork_thread;
    #[path = "pending_input.rs"]
    mod pending_input;
    #[path = "resume.rs"]
    mod resume;
    #[path = "resume_warning.rs"]
    mod resume_warning;
    #[path = "rollout_list_find.rs"]
    mod rollout_list_find;
    #[path = "sqlite_state.rs"]
    mod sqlite_state;
    #[path = "stream_error_allows_next_turn.rs"]
    mod stream_error_allows_next_turn;
    #[path = "stream_no_completed.rs"]
    mod stream_no_completed;
    #[path = "turn_state.rs"]
    mod turn_state;
    #[path = "window_headers.rs"]
    mod window_headers;
}
