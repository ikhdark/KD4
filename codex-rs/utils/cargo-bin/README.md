# Cargo Test Binary And Resource Resolution

`codex-utils-cargo-bin` centralizes Cargo test helpers used across the Rust
workspace.

- `cargo_bin` reads Cargo's `CARGO_BIN_EXE_*` environment variables and falls
  back to `assert_cmd` when necessary.
- `find_resource!` resolves fixtures relative to the consuming crate's
  `CARGO_MANIFEST_DIR`.
- `repo_root` walks from this crate's checked-in `repo_root.marker` to the
  workspace repository root.

These helpers are intended for test code. The packaged Codex CLI remains a
standalone binary with no dependency on repository resources.
