# Repository scripts

Keep executable paths small. Build wrappers must save wall-clock time; policy
gates, validation receipts, evidence ledgers, static oracles and audit inventories
do not belong here.

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
Explicit `--freeze-bundle` builds also select that mirror automatically when
eligible, including live checkouts, so edits after the copy cannot invalidate
the compiled bundle. Ordinary checkout builds without freezing stay in place;
`--source-mirror never` remains an explicit opt-out. Relocation/configuration
checks and in-place source-change rejection remain unchanged.

## Cargo artifact receipts

Freezing or reusing a bundle prints its `Build receipt:` path on stderr.
`just cargo-artifacts cargo build --release -p codex-cli` prints the existing
`build-receipt.json` to stdout for the exact source/settings/command match,
without building. It exits 1 if there is no match. Ordinary checkout builds do
not freeze by default: request a bundle with `run-lane --freeze-bundle NEW_DIR`
when one is needed.

`run-lane` emits one `Build reuse:` diagnostic on stderr and retains it in
`--timing-json` under `cacheReuse`; `find-bundle` emits `Frozen-binary reuse:`.
Frozen-binary decisions name the failed gate and expected/observed identities.
Missing or unreadable evidence is not a confirmed mismatch. An indexed composite
key mismatch does not identify which source, compiler or setting differed.
Cargo JSON artifact `fresh` observations are reported separately when captured;
they do not explain Cargo's dirty gate or compiler-cache behavior. Without an
invocation-scoped compiler-cache receipt, that cache's outcome remains unobserved
(shared counters cannot attribute a build). Frozen reuse runs neither Cargo nor
the compiler cache; absent compiler identity cannot establish an exact bundle hit.

For a snapshot, set `$KD4` and `$SNAPSHOT` to their absolute repository roots,
run from `$SNAPSHOT\codex-rs`, and supply the exact original Cargo command
(replace the example arguments with those used for that build):

```powershell
python "$KD4\scripts\build\rust_build_status.py" find-bundle --repo-root "$SNAPSHOT" --receipt -- cargo build --release -p codex-cli
```

`find-bundle` without `--receipt` still prints only the bundle directory to
stdout; the receipt location is on stderr. The receipt's `binaries` entries
already contain `name`, `sha256`, `bytes`, and `cargoPath`. Join each `name` to
the bundle directory for the frozen executable path. Use that copy, not the
shared Cargo path, which can change on later builds. Share the existing receipt
with the bundle rather than creating another artifact manifest. Legacy lookup
(`--include-legacy`) cannot be combined with `--receipt`: older provenance is
not a frozen build receipt.

## Other workflows

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
