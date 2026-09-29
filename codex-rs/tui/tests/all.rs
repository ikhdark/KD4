#![allow(clippy::expect_used)]

// Single integration test binary that aggregates all test modules.
// The submodules live in `tests/suite/`.
#[path = "../src/test_backend.rs"]
mod test_backend;

mod suite;
