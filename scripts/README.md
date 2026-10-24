# Repository scripts

Keep executable paths small. Validation/test wrappers must save wall-clock time;
policy gates, receipts, evidence ledgers, static oracles and audit inventories do
not belong here.

- `build/`: warm Cargo lane reuse, shared sccache/linker setup, bounded process
  execution and the opt-in LTO wall-clock benchmark. Lane commands execute once;
  they do not publish validation receipts or enforce test-selection policy.
- `maintenance/`: parallel formatting with explicit language/package selection.
- `install/`: install and publish the CLI, Code Mode host and sandbox helpers.
- `codex_package/`: construct and stage distributable packages.
- `common/`: shared file locking, atomic writes and process-tree cleanup.

Run Cargo directly from `codex-rs`, or use `just test-fast`,
`just test-lane-package <package>` and `just cargo-lane <lane> ...` to reuse
bounded warm build directories. Build any helper binaries required by the selected
tests explicitly. Named target/gate runners and automatic consumer expansion
have been removed.

Formatting remains opt-in: `just fmt` selects Just and Rust;
`just fmt-full` runs the independent formatters in parallel.
The Python project retains only formatting and archive dependencies.
Saved-session analysis belongs in KDR, not compatibility shims in this tree.
