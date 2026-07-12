# Workflow Strategy

`blocking-ci.yml` is the single merge-blocking and `main` entrypoint. It combines
fast change-aware checks with the full Cargo cross-platform suite.

## Pull Requests

- `rust-ci.yml` keeps change-aware checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes
- `rust-ci-full.yml` supplies the required broad Cargo signal:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests

## Main Branch

The same `blocking-ci.yml` family runs after pushes to `main`, so merged commits
receive the same Cargo checks required for pull requests.

## Rule Of Thumb

- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Put broad Rust build, test, and clippy coverage in `rust-ci-full.yml`.
- Keep platform-specific validation in the reusable Cargo/nextest workflows it owns.
