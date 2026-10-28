# Repository scripts

Keep executable paths small. Build wrappers must save wall-clock time; policy
gates, receipts, evidence ledgers, static oracles and audit inventories do not
belong here.

- `build/`: warm Cargo lane reuse, shared sccache/linker setup, bounded process
  execution and the opt-in LTO wall-clock benchmark. Lane commands execute once;
  they do not publish validation receipts or enforce command-selection policy.
- `maintenance/`: parallel formatting with explicit language/package selection.
- `install/`: install and publish the CLI, Code Mode host and sandbox helpers.
- `codex_package/`: construct and stage distributable packages.
- `common/`: shared file locking, atomic writes and process-tree cleanup.

Run Cargo directly from `codex-rs`, or use
`just cargo-lane <lane> cargo <command> ...` to reuse bounded warm build
directories. Resolved lanes include a source-root hash: legacy target caches are
retained but not adopted, preventing cross-checkout timestamp-only reuse. Use
the emitted target directory rather than deriving it from the requested name.
Sealed snapshots (no `.git`) built with `rust_build_status.py run-lane --repo-root
SNAPSHOT` reuse an exact frozen bundle, or build in the nearest free warm slot
through a content-synced source mirror, and freeze their executables with
`build-receipt.json` (`--freeze-bundle NEW_DIR`, else the local bundle store).
`find-bundle` answers without building; `--include-legacy` covers older bundles.
Repository test suites, test-only support files, and named test and
coverage recipes have been removed. Generic Cargo tooling still accepts
caller-selected commands without supplying a repository test suite.

The opt-in LTO runtime benchmark generates a disposable coding workload and its
behavior oracle outside the repository source tree. That benchmark input is not
a retained repository test suite; see `build/lto-runtime-benchmark.md`.

Formatting remains opt-in: `just fmt` selects Just and Rust;
`just fmt-full` runs the independent formatters in parallel.
The `scripts/` Python project retains only formatting and archive dependencies.
Saved-session analysis belongs in KDR, not compatibility shims in this tree.
