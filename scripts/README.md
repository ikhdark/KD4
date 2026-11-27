# Repository scripts

Start with the [repository overview](../README.md) for the documentation index
and [AGENTS.md](../AGENTS.md) for workflow policy. Commands below describe the
available tooling; they do not authorize provisioning, publishing or workloads
disallowed by the active validation mode.

Keep executable paths small. Build wrappers must save wall-clock time; policy
gates, validation receipts, evidence ledgers, static oracles and audit inventories
do not belong here.

- `build/`: warm Cargo lane reuse, direct compiler/linker setup, bounded process
  execution. Lane commands execute once; they do not publish validation receipts
  or enforce command-selection policy.
- `maintenance/`: parallel formatting with explicit language/package selection.
- `install/`: install and publish the CLI and Code Mode host.
- `codex_package/`: construct and stage distributable packages.
- `common/`: shared file locking, atomic writes and process-tree cleanup.
- [`inference/`](inference/README.md): opt-in, loopback-only presets for a pinned
  local llama.cpp engine. These do not change the KD4 provider or model.

Run Cargo directly from `codex-rs`, or use
`just cargo-lane <lane> cargo <command> ...` from the checkout root to reuse
bounded warm build directories. Direct `rust_build_status.py run-lane` calls do
not change cwd: run them from the source's `codex-rs` directory so its pinned
toolchain and Cargo configuration apply. Resolved lanes include a source-root
hash: legacy target caches are retained but not adopted, preventing
cross-checkout timestamp-only reuse. Use
the emitted target directory rather than deriving it from the requested name.
Windows lane owners preflight representative native assembler output paths before
Cargo starts. An overlong default uses the owned private root
`%USERPROFILE%/.kd4-lanes/<checkout-id>`; explicit roots are never redirected.
Short explicit `--lanes-root`/`-LanesRoot` values are initialized by the same
ownership-marker guard. The full source-root digest remains in the lane name.
Use the emitted root explicitly for status/pruning of a relocated lane; legacy
roots and their warm artifacts are left untouched.
Sealed snapshots (no `.git`) built with `rust_build_status.py run-lane --repo-root
SNAPSHOT` reuse an exact frozen bundle, or build in the nearest free warm slot
through a content-synced source mirror, and freeze their executables with
`build-receipt.json` (`--freeze-bundle NEW_DIR`, else the local bundle store).
`find-bundle` answers without building; `--include-legacy` covers older bundles.
Frozen reuse binds the effective inherited build environment because Rust,
macros and build scripts can read variables not named in Cargo's options.
The receipt's `buildEnvironmentSha256` records the digest, not the inherited
values. It excludes the three variables set by the lane owner:
`CARGO_TARGET_DIR`, `CODEX_CARGO_LANE_TARGET_DIR`, and `CODEX_CARGO_LANE_OWNER_PID`.
A changed environment or a legacy receipt without the digest cannot establish
an exact frozen match. `run-lane` falls through to normal Cargo freshness;
ordinary warm-lane scheduling is unchanged. This does not attest to inputs
created or read outside the inherited environment during compilation.
Explicit `--freeze-bundle` builds also select that mirror automatically when
eligible, including live checkouts, so edits after the copy cannot invalidate
the compiled bundle. Ordinary checkout builds without freezing stay in place;
`--source-mirror never` remains an explicit opt-out. Relocation/configuration
checks and in-place source-change rejection remain unchanged.

In-place Python builds wait for busy reusable lanes by default. Eligible
mirrored Python builds allow cold siblings when capacity remains, but for Cargo
build/check they can wait up to 30 additional seconds if a compatible managed
lane is expected to finish sooner. Successful lane timings are advisory;
resource admission is still authoritative, and no running build is preempted.
`--warm-wait-seconds` sets the minimum delay before overflow; `--no-cold-overflow` waits for a
reusable lane rather than creating a cold sibling behind it. Mirrored and
overflow-enabled families default to eight active lanes; `--max-active-lanes`
can override the Python limit. PowerShell preserves its bounded busy exit
and applies the same environment cap when `-AllowColdOverflow` is selected.
Eligible source-mirror builds enable bounded overflow automatically. The short
`--lock-timeout-seconds` covers coordination-lock acquisition, not build time.

### Shared Windows build resources

Both `run-lane` and `cargo-lane.ps1` use one admission pool across lane families,
source checkouts, and KDR's copied runner. Existing lane locks, warm-cache ranking,
bundle reuse, and cleanup quarantine remain independent safeguards. Exact frozen
bundle hits need no resource lease. Direct unmanaged Cargo commands do not join
this pool; their memory use still reduces the observed admission headroom.

Cargo jobs are chosen at launch: at 32 logical processors, an otherwise idle pool
starts with 32, then 16, 10, 8, 6 and 5 jobs as builds join. Explicit smaller
job counts remain smaller. Running Cargo job counts cannot be changed safely;
Windows owning jobs instead rebalance **whole-process-tree hard CPU caps** as
builds join/finish. Six builds have at most one sixth of the configured CPU
ceiling each. Opaque wrappers receive `CARGO_BUILD_JOBS`; wrapper-supplied `-j`
may override it, but not the owning tree's CPU cap. Nested managed wrappers reuse
a verified parent lease.

Defaults are experimental: at most six managed builds (including cold builds),
100% aggregate host CPU, 512 MiB projected commit per Cargo job, plus 4 GiB for
a cold build or 2 GiB for a warm build. Admission leaves at least 2 GiB each of
available physical memory and commit headroom, subtracting other admitted builds'
projected growth (not double-counting their current commit). Insufficient headroom
reduces launch jobs or waits. These estimates are not hard memory limits or proof
that another build would improve makespan. Cold builds reserve more projected
memory. Set `MAX_COLD` below `MAX_ACTIVE` for an explicitly measured cold-build
concurrency experiment; it defaults to the active-build ceiling rather than
silently serializing otherwise admissible cold work.

Refused requests are admitted in arrival order: each keeps a ticket under
`.builds/resources/waiters/`, refreshed on every admission pass, and a later
request that fits waits (`earlier_waiter`) while an older waiter would also fit;
the older request's projected reservation is kept for it. An older request still
refused by a cap or headroom never holds back a later one that fits, and a ticket
not refreshed for 1 s (dead, cancelled or lane-blocked owner) stops counting.
A busy PowerShell exit withdraws its ticket. Runner copies without this change
join the pool unordered. `CODEX_CARGO_BUILD_FIFO=off` restores unordered admission.

Use process-scoped environment overrides, not machine-wide settings:

```powershell
$env:CODEX_CARGO_LANE_MAX_ACTIVE_PER_BASE = "3" # cap a mirror or overflow-enabled family
$env:CODEX_CARGO_BUILD_MAX_ACTIVE = "6"       # shared admission ceiling
$env:CODEX_CARGO_BUILD_CPU_PERCENT = "100"    # aggregate CPU cap, 1..100
```

Explicit cleanup defaults to one idle compiled lane per family; empty reservations
do not use that quota. KD4's existing automatic maintenance retains two by default
and still honors an explicit `CODEX_CARGO_LANE_MAX_ACTIVE_PER_BASE` setting.

Other positive-integer `CODEX_CARGO_BUILD_` settings are `MAX_COLD`,
`MEMORY_PER_JOB_MIB`, `COLD_BASE_MIB`, `WARM_BASE_MIB`,
`PHYSICAL_RESERVE_MIB`, and `COMMIT_RESERVE_MIB`. Concurrent callers use the
strictest active CPU/count/reserve settings. The pool and per-build telemetry
receipts live under `KDR/.builds/resources/`. `RESOURCE_ROOT`
overrides that location for an isolated measurement; different roots are separate
budgets, so ordinary callers must share the default. No service or provisioning
is needed. Admission sleeps without holding coordination; cancellation and
process-tree cleanup retain the existing owner. Python releases its lane between
resource-admission probes, avoiding a lane/resource lock-order cycle. PowerShell
returns exit 75 with `executed=false`/validation pending when resources are busy,
as it already does for busy lanes; it never queues while holding a lane. Unconfirmed cleanup quarantines
the resource lease as well as its lane; inspect the recorded tree before manual
recovery. If a secondary settlement operation fails, the Python lane runner still
reports the primary cleanup failure, with the secondary error as a note. Do not
assume a quarantine marker or receipt was written if its write failed. Crashed
owners' leases are reclaimed only after their named tree is gone.

`--timing-json` adds `laneAdmissionMs`, `laneWaitMs` and `resources`: resource-admission wait, chosen
jobs/policy, native job CPU seconds/utilization, lifetime peak tree/process commit,
fault counts, and 0.5-second system CPU/memory/paging samples. Receipts are also
emitted for PowerShell builds. PDH page input/output rates are **system-wide**;
job faults include soft faults. Process commit is not resident working set or
pagefile traffic. System memory peaks are sampled, not lifetime peaks. Unavailable
paging counters carry an error, never a fabricated zero. For Python, `laneAdmissionMs`
is the entire lane/resource admission phase; `laneWaitMs` subtracts the resource
queue interval (which includes retried lane probes). `cargoMs` starts after this
phase; `resources.compileTreeMs` times the owned tree. Resource capture
and CPU control add overhead, particularly for already-fresh short checks.
Python run-lane receipts also carry `laneReservation`: time per wait phase
(coordination-lock, lane-selection, resource-admission, retry-wait), pass counts
per outcome, coordination-lock grace expiries and the last holder. The record is
kept when the wait fails. Lane reservation has no runner deadline, so it ends
without a lane only when admission raises, the lock file stays access-denied
past its limit, or the process is interrupted. `openPhaseMs` then times the phase
the wait ended in, up to receipt finalization; it is not where the wait spent
its time. A wait past 60 s prints one progress line per minute. A lane is warm
when it holds Cargo fingerprints in the `.fingerprint`/`deps` layout or the newer
`build/<package>/<hash>/fingerprint` layout. The reservation is cold unless the
lane is warm and its recorded build context matches.

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
when one is needed. For behavioral verification amid concurrent edits, follow
[KDR's build handoff](../../KDR/scripts/build/README.md#concurrent-verification-handoff).
The managed runner warns on Cargo builds that leave only mutable lane outputs.
For behavioral verification, run the frozen executable, not the mutable one.

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

For actual invocation freshness, use `run-lane --timing-json FILE --observe-units
-- cargo build ...` (or `cargo check`). This opt-in executes Cargo even if a
frozen bundle is available, recording per-artifact freshness, features, manifest
and outputs. Cargo reports the freshness decision, not its cause.

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

## Deterministic source editing

Use an existing generator or formatter when the desired output is mechanical.
For a standalone Rust binary, `cargo init --bin --vcs none --edition 2024
--name NAME PATH` creates the starter manifest and main without creating a
test scaffold. Use a new or inspected isolated destination, then review the
generated files before integrating them. For Python formatting, use an existing
`ruff format PATH` scoped to the intended file and its project settings rather
than authoring a whitespace patch.

When syntax determines the scope of a rewrite:

- [Python AST preview](maintenance/python_ast_patch.py):
  `python python_ast_patch.py PATH SOURCE_SHA256 EDITS_JSON`. Supported keys
  are `import.MODULE` (replace a one-module top-level import), `assignment.NAME`
  (replace a top-level literal), and `function.FUNCTION.assignment.NAME`
  (replace the literal right operand of a binary expression in a direct named
  assignment). Missing, duplicate and unsupported selectors refuse the plan.
  Short literal patches are used only when native matches select the intended
  AST spans; otherwise the preview uses a guarded whole-source replacement.
- Use an installed `ast-grep scan --inline-rules RULE --json=compact PATH` to
  preview a structural rewrite. Target the smallest syntax node (for example
  the name of a keyword argument under a specific receiver call, not the whole
  call). Use the returned source byte spans only against the exact captured
  bytes, and publish through a native guarded patch. Do not use regex to
  substitute matching spellings in comments, strings or different receivers.
- [TypeScript language-service preview](maintenance/typescript_patch.cjs):
  `node typescript_patch.cjs SPEC_JSON [trusted-TypeScript-module]`. From the
  project directory provide `files:[[relativePath,sourceSha256],...]` for a
  complete, explicitly enumerated standalone TypeScript project (1–64 UTF-8
  `.ts` files; no config/dependency discovery). Plans contain `op` and `file`:
  `rename` takes `symbol` and the new identifier in `value`; `move` takes the
  absent target path in `value`; `signature` takes `symbol` and converts
  supported positional parameters and callers to a destructured object.
  Use an already installed trusted TypeScript module. Unresolved module
  references and edits outside the supplied project are refused.

The Python and TypeScript previews write only a patch to stdout. They choose
among verified local literals, a covering span and the whole file, minimizing
encoded bytes among those representations while retaining the source guards.
Check successful, exact stdout and submit it to native `apply_patch` if intended;
continue only after inspecting its receipt. The previews do not apply or retry
edits. Currency is checked on preview and native application. Native guards are
preconditions, not multi-file atomic publication. For range edits and repeated
literal substitutions see the [Code Mode recipe reference](../codex-rs/code-mode-protocol/src/description/exec_reference.md#deterministic-recipes);
for strict JSON use the existing
[native JSON operations](../codex-rs/prompts/templates/apply_patch_tool_instructions.md)
instead of reserializing the file.

## Offline engineering artifacts

Run [engineering_artifacts.py](maintenance/engineering_artifacts.py) with Python 3.13+
and explicit local inputs (also callable through native Code Mode `exec_command`).

- `python scripts/maintenance/engineering_artifacts.py lock-diff BEFORE AFTER`
  parses Cargo v3/v4 files and reports package IDs, resolved dependencies and
  checksum changes. Ambiguous dependency references are refused; no fetch,
  workspace resolution, or write is performed.
- `python scripts/maintenance/engineering_artifacts.py migrate INPUT.json RECIPE.json`
  emits the migrated JSON without changing its input. A recipe is an object
  with integer `from` and `to` versions and an ordered `operations` array
  (`add`, `replace`, `remove`, or `move`, using object-key JSON Pointers).
  The command checks `schema_version`, refuses destination collisions, and
  advances the version only after all operations succeed.
- `python scripts/maintenance/engineering_artifacts.py bundle ENTRIES.json NEW.zip`
  takes an explicit JSON object mapping portable archive names to absolute file
  paths. It produces a new ZIP and `manifest.json` with member sizes and SHA-256
  hashes. Select only permitted, shareable inputs: the command does not discover
  sources, redact bytes, execute recorded commands, or overwrite an archive.

## Other workflows

Repository test suites, test-only support files, and named test and
coverage recipes have been removed. Generic Cargo tooling still accepts
caller-selected commands without supplying a repository test suite.

Formatting remains opt-in: `just fmt` selects Just and Rust;
`just fmt-full` runs the independent formatters in parallel.
`--rust-package PACKAGE` scopes Rust formatting; `--only GROUP` selects an
explicit formatter group. The `pnpm format` family checks formatting;
`pnpm format:fix` writes it. Check recipes remain subject to validation policy.
A direct `rustfmt FILE` rewrites every hunk of FILE and of its out-of-line
child modules, including concurrent edits. For a shared file, run it with
`--check` (no writes) and apply only the owned hunks.
The `scripts/` Python project retains only formatting and archive dependencies.
`just sdk-python-check` first synchronizes development dependencies into
`.cache/sdk-python-check` (excluding the pinned CLI runtime), then runs Ruff.
The Python formatter groups also use `uv run`, which can synchronize their
environments. A check or formatting recipe is not an offline/no-install guarantee;
inspect its workload and follow setup permissions before running it.
Saved-session analysis belongs in KDR, not compatibility shims in this tree.
