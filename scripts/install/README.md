# Install Scripts

Repository instructions live in the root [AGENTS.md](../../AGENTS.md). This README is a
human-facing overview of the installer surface.

## Overview

This directory owns the Windows standalone Codex install entrypoint,
`install.ps1`.

The installer fetches KD4 release artifacts from `ikhdark/KD4`, verifies release
digests, stages standalone package layouts under the Codex home directory,
exposes the selected binary on PATH, and handles conflicts with existing npm,
bun, or older standalone installs. Set `CODEX_RELEASE_REPOSITORY=owner/name` to
use another explicitly selected fork release source.

The standalone package layout is supported from Codex `0.133.0` onward. The
installer rejects older releases before requesting release assets; the retired
platform npm archive layout is not probed or installed.

## Build profiles

The configured Cargo profiles and publisher `-Profile` values are `dev`,
`release` (final), and `pgo`. The publisher defaults to `release`.

## Final builds

`release` uses optimization level 3, no debug info (`debug = 0`), Thin LTO,
one codegen unit for workspace crates and runtime dependencies, and incremental
compilation disabled. Build scripts, proc macros, and build-only dependencies
retain Cargo's build defaults. `pgo` inherits these settings.
Windows builds retain native CPU tuning,
the 8 MiB stack, and static CRT from the shared Cargo configuration.

The local publisher explicitly enables `/OPT:REF` (unused-code removal) and
`/OPT:ICF` (identical COMDAT folding) for Windows MSVC `release` builds and
both PGO passes. These flags join the shared target flags; dev publishes are
unchanged. Direct Cargo builds do not use this publisher overlay.

`just publish-local-codex-final` remains the normal local publish command. It uses
the `release` profile and `codex-rs/target/publish-release` build directory; `cargo build --release`
uses the same optimization settings but does not install or restart Desktop.

Use `just publish-local-codex-final` (or `-Profile release`) for final publishing.

## Local publishing

The publisher builds the CLI, Code Mode host, and both sandbox helpers,
including freshness checks, rollback, Desktop configuration, and restart
safety. Desktop uses the fork home (`C:\Users\kuh\Desktop\LOCAL-KD`), not the
upstream home. Publishing can restart Desktop; `-BuildOnly` only prepares the
bundle. Existing protections may defer activation when invoked inside Codex.
`just publish-local-codex-final` remains unchanged for runtime-optimized builds.

Build freshness includes the bytes of the automatically selected Cargo Rusty V8
cache archive, not just explicit archive overrides. A cached archive change
invalidates auto-skip even if file timestamps are preserved.

Before activation, close processes using any of the four installed bundle
executables; before restart, close Desktop as well. The publisher cannot prove
home ownership or live chat state from an executable path, so it refuses to stop
even apparently idle processes without `-Force`. That switch explicitly permits
interruption across **all homes** sharing those executables. Historical unfinished
rollouts do not block publishing. `-AllowRunningTarget` still skips bundle-process
shutdown; it does not authorize stopping Desktop or bypass Windows file locks.

Recovery retains the journal and reports an error if an unverified activation
needs a previous installation whose rollback directory is missing. Do not delete
that journal to bypass recovery: restore the previous bundle through manual
recovery first. Mutex ownership is released even when recovery fails.

## Profile-guided optimization (PGO)

`publish-local-codex-pgo.ps1` orchestrates four explicit stages. Running
`just publish-local-codex-pgo` defaults to **Prepare**, not installation. It uses
the existing publisher for Cargo builds, build stamps, transactional installation,
fork-home routing, rollback, and live-chat/restart protection. The final recipe
remains available unchanged. No speedup is assumed or promised.

Install the matching profiler tools once, from `codex-rs` (not a random system LLVM):

```powershell
rustup component add llvm-tools-preview
```

Then run from the checkout root:

```powershell
just publish-local-codex-pgo -Stage Prepare
just publish-local-codex-pgo -Stage Train -TrainingScript C:\workloads\train-kd4.ps1
just publish-local-codex-pgo -Stage Optimize
# Outside Codex, after other chats finish:
just publish-local-codex-pgo -Stage Publish
```

Prepare builds an instrumented `pgo` bundle and copies the CLI, Code Mode host, and
sandbox helpers into a private training bundle. It cannot install that bundle.
Train runs **your selected PowerShell script**, which must perform representative
workloads, check their outcomes, fail with a nonzero exit code on failure, and wait
for all child processes to exit normally. It receives these process-only variables:

- `CODEX_CLI_PATH`: the instrumented CLI; invoke this, not `codex` from PATH.
- `CODEX_CODE_MODE_HOST_PATH`: its matching instrumented Code Mode host.
- `CODEX_HOME`/`CODEX_SQLITE_HOME`: the selected fork home and its SQLite directory.
- `LLVM_PROFILE_FILE`: a fresh, per-training-run `.profraw` output pattern.

For example, your training script could contain the following, with your own
representative prompts and independent outcome checks:

```powershell
Set-Location C:\workloads\disposable-repository
& $env:CODEX_CLI_PATH exec 'Inspect the parser and explain how malformed input is handled.'
if ($LASTEXITCODE -ne 0) { throw 'Training task failed' }
# Add representative read/edit/tool/Code Mode tasks; verify their actual outcomes.
```

Training can invoke the model, consume tokens, and modify its selected workload
workspace. Use disposable workspaces; do not edit this checkout during training.
The wrapper does not launch Desktop, copy credentials, or invent a training corpus.
`doctor`/`--version` alone are not representative training. Normal fork-home
credentials/configuration remain in use unless you select `-LocalCodexHome`.

Optimize merges only the last successful training run with the pinned toolchain's
`llvm-profdata`, then builds optimized `pgo` binaries without installing them. Both passes
use the same `pgo` Cargo profile (inheriting `release` settings), with
incremental compilation disabled and separate, explicitly targeted Cargo output
directories and build stamps. `publish-pgo-instrumented` is an output directory,
not another profile. `-PgoGenerateDirectory` selects instrumentation;
`-PgoProfileData` selects optimization, and the publisher requires exactly one.
Using the same Cargo profile preserves crate identities across the
instrumentation/use passes. Windows stack,
static CRT, and native-CPU flags are preserved; sccache is bypassed. Ambient global
Rust flags and Cargo profile/incremental overrides are rejected.

Source/toolchain/settings drift, changed training bundles/profiles, failed or
still-running training, and mismatched optimized artifacts block the next stage.
Changed source requires Prepare and Train again. Merged profiles use SHA-256-named
files, and the publisher also hashes their contents, so auto-skip cannot silently
reuse another profile. No stages run concurrently in the same `-WorkDir`.

State, raw profiles, and immutable training bundles are retained under
`codex-rs/target/pgo` by default. Use the same optional `-WorkDir` for every stage;
it may be elsewhere outside the checkout or beneath `codex-rs/target`. Old runs are
not recursively deleted. Keep the same `-RustyV8Archive`/environment settings across
stages if you override them. Publish accepts `-InstallDir`, `-LocalCodexHome`, and
the existing explicit `-Force` interruption override; normally close runtime and
Desktop processes first, rather than merely waiting for idle chats.
Activation may still be deferred when called inside Codex.

Compare the optimized bundle against a same-source normal final build on separate,
held-out tasks before publishing. Use KDR for real session comparisons. PGO targets
local Rust CPU work, not model/network latency. The LTO fixture is not by itself a
suitable PGO training corpus.
