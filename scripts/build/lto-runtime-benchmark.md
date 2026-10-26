# KD4 LTO runtime benchmark

## Recommended runtime screening

```powershell
python -B scripts/build/benchmark_lto_runtime.py --matrix screening --experiment-dir codex-rs/target/runtime-screening
```

This compares five same-source candidates: the current `local-release` profile
(baseline), that baseline with native CPU targeting, FatLTO, FatLTO with
one workspace codegen unit, and the baseline at optimization level 2. The
FatLTO/one-unit comparison is against FatLTO/16 units, not an isolated LTO test
against the baseline. Native builds append to target-specific Rust flags,
preserving the configured Windows stack and static CRT; conflicting global
`RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` are rejected rather than silently replacing
those settings. Native executables are for the build machine, not distribution.

The optional experiment directory is exclusively locked without waiting. It
retains frozen source, compiler/settings identity, and hashes of completed build
bundles. Repeating the exact command reuses verified completed builds and
finishes any unrecorded build; it never merges partial runtime samples. Modified
source/binaries or different compiler/settings are rejected. Use a new empty
directory for a new experiment. An interruption during initial staging can
leave an unowned partial copy: it is deliberately not adopted or deleted.
`result.json` is the last **complete** summary, with a completion timestamp;
its existence does not prove a later interrupted attempt completed. No raw
session logs or sample ledger is written. Omit the option for temporary files.

The JSON summary also includes valid core turn, orchestration and stream
processing elapsed times when the executable provides them. Missing timing is
unknown, never zero; invalid/incomplete profiles fail the comparison. These
are distinct, potentially overlapping diagnostics, **not CPU time** and not a
complete isolation of harness overhead. Full process wall-clock remains the
primary metric. Each metric has its own observed-sample count.

PGO publishing has a separate [staged workflow](../install/README.md#profile-guided-optimization-pgo)
that requires a user-supplied training script. This fixture alone is not a
responsible PGO training corpus; use representative training and separate held-out
tasks. Allocator, panic/unwinding, toolchain, affinity and power settings are
not changed by this screening. No candidate is installed automatically.

## Current configuration versus LTO candidates

Run from the checkout root:

```powershell
python -B scripts/build/benchmark_lto_runtime.py
```

This builds **the actual `codex` CLI and `codex-code-mode-host`**, sequentially
through the existing `lto-runtime` build lane: `baseline` and `fat`.
The baseline inherits the frozen source's `local-release` profile without any
profile overrides. Currently that means `lto = "thin"`, eight codegen units for
workspace crates, one for dependencies, and incremental compilation disabled. Windows
target flags also enable `target-cpu=native` for every variant. There is no separate LTO-disabled
candidate. The baseline is rebuilt from matched source, not taken from the
installed executable. Ambient `CARGO_PROFILE_*` and `CARGO_INCREMENTAL` overrides
are cleared for all builds; baseline means repository configuration.

The FatLTO candidate also inherits `local-release`, but overrides LTO,
speed optimization to 3, workspace codegen units to 16, and incremental
compilation to off. Thus comparison against baseline is a configuration
comparison, not an isolated LTO experiment. The native CPU screening candidate
is redundant with the current Windows baseline; opt-level 2 changes only that
setting relative to baseline. Null variant fields
in the report mean inherited settings, not disabled optimization.
Dependency overrides are inherited unchanged. No tracked
profile, installed executable, Desktop configuration, or shared cache setting is
changed. The lane returns busy rather than waiting; do not interpret that as a
completed benchmark. Compilation can take substantial time and disk space, but
**build time never contributes to the result**.

To compare FatLTO at one workspace codegen unit instead, leaving the
baseline unchanged:

```powershell
python -B scripts/build/benchmark_lto_runtime.py --codegen-units 1
```

Keep results from those two experiments separate: changing both LTO and codegen
units is not an isolated test of LTO. Dependencies retain their inherited
codegen-unit override.

## What actually runs

This is a performance benchmark, not a repository test suite. Its behavior
oracle is generated only inside each disposable workload workspace.

Each sample launches a fresh `codex exec` process and matching real Code Mode
host. The bundled model catalog is snapshotted once and supplied to every run
so native tool capabilities do not depend on provider discovery or a user cache.
A fixed local Responses/SSE provider issues three Code Mode cells:

1. Read two source/test files using real native `read_file` tools.
2. Fix an aggregation bug using real native `apply_patch`.
3. Launch Python through real `exec_command` and run six behavior tests.
4. Receive the test result and finish the turn with the exact required answer.

The fixture includes repeated/interleaved categories, negatives, zero, empty
input, and a single category. The original code fails four of six tests. The
benchmark independently checks the final source and unchanged test file and
reruns all six tests outside KD4. A scripted success message cannot pass by
itself. Failed/timed-out runs, extra requests, missing tool outputs, modified
oracles, and changed binaries abort the comparison; they are never scored.

Timing covers process launch, harness initialization, prompt preparation,
stream processing, Code Mode/host dispatch, tools, result relay, turn completion,
process exit, and owned-child cleanup. Fixture/server setup and independent
verification are outside the timer. Session persistence remains enabled in an
isolated temporary `CODEX_HOME`; existing user sessions and credentials are not
used. Execution is unsandboxed **only in a disposable fixture workspace**, so
this does not benchmark sandbox-helper overhead.

Both 4 KiB and 512 KiB prompt-padding workloads run. Each gets one discarded
warmup and six measured rounds by default. Rotated/reversed orders reduce
order bias without factorial enumeration. Use a multiple of the candidate count
for measured rounds to balance variant positions (for example, `--rounds 5`
for the default screening matrix). The summary reports median, min, max and count separately
for each prompt size. `--json` prints a summary with executable hashes, source
fingerprint, settings, and fixed work counts; no raw session/tool logs are saved.

The source guard stages one private copy of `codex-rs` and `third_party`, excluding
build output. It checks source hashes before/after copying and rejects observed
input drift. All builds use that same copy, so later edits in the shared
checkout cannot change their inputs. The copy is rechecked after each build and
deleted after a temporary experiment; an explicit experiment directory retains
it for resumption. Cargo artifacts remain in the lane for reuse.
Staging is not an atomic filesystem snapshot: edits changed back between hash
observations cannot be ruled out. The copied tree has no Git metadata; version
identity comes from its content hash, not a Git revision.

## Existing executable / smoke test

```powershell
python -B scripts/build/benchmark_lto_runtime.py --bundle current=C:\path\to\bundle --rounds 1 --warmups 0
```

A bundle directory must contain both `codex.exe` and `codex-code-mode-host.exe`.
Repeat `--bundle LABEL=DIRECTORY` to compare up to six existing builds. Labels
are not evidence of compiler flags or matched source; only the default build
path controls those. One sample is a functional smoke test, not a performance
conclusion. `--timeout` bounds each task; process-tree cleanup uses the existing
owned-process helper.

## Interpretation

Lower runtime is better **on this workload**. Do not automatically pick a winner
from tiny differences or overlapping ranges; rerun under stable machine load.
Build all variants before timing; no compilation runs inside a measured interval.

This is a real harness execution benchmark with a deterministic model script,
not a compiler microbenchmark and not live model inference. It does not measure
native Desktop UI, Internet/provider latency, model reasoning quality, arbitrary
coding tasks, or multi-agent coordination. It cannot establish the universally
fastest `.exe` or a live-task speedup. For those claims, additionally compare
matched real coding sessions through KDR with independently verified outcomes.

