#![allow(clippy::unwrap_used, clippy::expect_used)]

// Re-export backend OpenAPI models. They originate from generated code, but
// `src/models/mod.rs` is now a curated export list and some model files carry
// hand-maintained types (for example the open fallback enums in
// `rate_limit_status_payload.rs`); a wholesale regeneration would drop them.
pub mod models;
