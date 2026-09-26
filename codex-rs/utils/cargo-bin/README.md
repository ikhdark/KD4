# Cargo Test Binary And Resource Resolution

`codex-utils-cargo-bin` centralizes Cargo test helpers used across the Rust
workspace.

- `cargo_bin` reads `CARGO_BIN_EXE_<name>`, then its underscored alias. A set
  variable is authoritative: a relative path, or one that does not name an
  existing file, is an error rather than a reason to fall back. Only when
  neither variable is set does it use the binary in the running test's profile
  directory, and it rejects that binary when Cargo's dep-info beside it records
  an input that changed or disappeared after the build.
- `find_resource!` resolves fixtures relative to the consuming crate's
  `CARGO_MANIFEST_DIR`.
- `repo_root` walks from this crate's checked-in `repo_root.marker` to the
  workspace repository root.

These helpers are intended for test code. The packaged Codex CLI remains a
standalone binary with no dependency on repository resources.
