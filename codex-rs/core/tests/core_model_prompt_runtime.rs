#![allow(clippy::expect_used)]

// Bounded replacement shard for the legacy `all` integration target.
// Model selection, prompt assembly, and turn-context runtime integration tests.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.
pub use codex_protocol::error;

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "additional_context.rs"]
    mod additional_context;
    #[path = "current_time_reminder.rs"]
    mod current_time_reminder;
    #[path = "image_rollout.rs"]
    mod image_rollout;
    #[path = "model_overrides.rs"]
    mod model_overrides;
    #[path = "model_runtime_selectors.rs"]
    mod model_runtime_selectors;
    #[path = "model_switching.rs"]
    mod model_switching;
    #[path = "model_visible_layout.rs"]
    mod model_visible_layout;
    #[path = "models_cache_ttl.rs"]
    mod models_cache_ttl;
    #[path = "override_updates.rs"]
    mod override_updates;
    #[path = "personality.rs"]
    mod personality;
    #[path = "prompt_caching.rs"]
    mod prompt_caching;
    #[path = "prompt_debug_tests.rs"]
    mod prompt_debug_tests;
    #[path = "quota_exceeded.rs"]
    mod quota_exceeded;
    #[path = "safety_buffering.rs"]
    mod safety_buffering;
    #[path = "web_search.rs"]
    mod web_search;
}
