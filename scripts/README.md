# Repository scripts

Start with the [repository overview](../README.md) for the documentation index
and [AGENTS.md](../AGENTS.md) for workflow policy. Commands below describe the
available tooling; they do not authorize provisioning, publishing or workloads
disallowed by the active validation mode.

Keep executable paths small. Build wrappers must save wall-clock time; policy
gates, validation receipts, evidence ledgers, static oracles and audit inventories
do not belong here.

- `build/`: warm Cargo lane reuse, shared sccache/linker setup, bounded process
  execution. Lane commands execute once; they do not publish validation receipts
  or enforce command-selection policy.
- `maintenance/`: parallel formatting with explicit language/package selection.
- `install/`: install and publish the CLI, Code Mode host and sandbox helpers.
- `codex_package/`: construct and stage distributable packages.
- `common/`: shared file locking, atomic writes and process-tree cleanup.

Run Cargo directly from `codex-rs`, or use
`just cargo-lane <lane> cargo <command> ...` from the checkout root to reuse
bounded warm build directories. Direct `rust_build_status.py run-lane` calls do
not change cwd: run them from the source's `codex-rs` directory so its pinned
toolchain and Cargo configuration apply. Resolved lanes include a source-root
hash: legacy target caches are retained but not adopted, preventing
cross-checkout timestamp-only reuse. Use
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

Busy reusable lanes wait for an owner to finish by default. `--warm-wait-seconds`
is the delay before explicitly enabled `--allow-cold-overflow`, not a total
wait timeout; `--max-active-lanes` bounds that family's concurrent builds.
Eligible source-mirror builds enable bounded overflow automatically. The short
`--lock-timeout-seconds` covers coordination-lock acquisition, not build time.

## Cargo artifact receipts

Opt-in `--timing-json` captures source/settings/compiler/runner identities and
before/after source stability under `timingEvidence`. `cargoMs` times the owned
Cargo invocation (including its process/cache preparation), separately from
`totalMs`, the wrapper timer including all metadata observations. Metadata
`captureMs` is already included in that total; do not add it to `cargoMs` or
compare newer wrapper totals with legacy totals as if capture were free. This capture
does not change lane admission, compiler settings, cache decisions or freezing.
Frozen reuse has no Cargo duration. KDR owns retained timing comparison.

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

Formatting remains opt-in: `just fmt` selects Just and Rust;
`just fmt-full` runs the independent formatters in parallel.
`--rust-package PACKAGE` scopes Rust formatting; `--only GROUP` selects an
explicit formatter group. The `pnpm format` family checks formatting;
`pnpm format:fix` writes it. Check recipes remain subject to validation policy.
The `scripts/` Python project retains only formatting and archive dependencies.
Saved-session analysis belongs in KDR, not compatibility shims in this tree.
