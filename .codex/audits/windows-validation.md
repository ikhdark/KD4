# Windows validation for KD4 audits

The **Windows validation** Actions workflow builds the current source on
`windows-2025` x64 MSVC and runs three existing manifest gates with
`scripts/build/rust_test_runner.py`. It runs automatically for pull requests and
pushes to `main`, and supports **Run workflow** (`workflow_dispatch`).

The focused suite is declared in `codex-rs/.config/kd4-rust-tests.toml`:

| Gate | Current tests | Coverage |
| --- | ---: | --- |
| `windows-process` | 8 | Pipe/ConPTY process descendants, terminal input, Ctrl-C, sandbox wait errors, command-runner IPC EOF |
| `capability-preferred-powershell-host` | 2 | Explicit shell selection and PowerShell fallback |
| `validation-command-recognition` | 4 | KD4 validation command and receipt classification |

The workflow separately builds both real `codex-windows-sandbox` helper binaries,
including the setup helper's embedded Windows manifest. It does **not** execute
elevated setup, require model credentials, launch a live Codex session, or build
the CLI/core or the separate V8 host. Extend coverage through existing targets
and gates when an audit touches a different owner; this suite does not validate
every potential fix.

## Launch and retrieve results

Create a branch with a candidate fix and open a PR against `main`. The workflow
checks out the immutable event SHA. For a PR this is GitHub's synthetic merge
commit, and evidence also records the PR head and base SHAs. A push to the PR
replaces the prior check; concurrency cancellation prevents superseded work
from consuming another full build.

After the workflow exists on the default branch, manually test a branch through
Actions → **Windows validation** → **Run workflow**, or:

```powershell
gh workflow run windows-validation.yml --repo ikhdark/KD4 --ref <candidate-branch>
gh run list --repo ikhdark/KD4 --workflow windows-validation.yml --limit 10
gh run view <run-id> --repo ikhdark/KD4 --log-failed
gh run download <run-id> --repo ikhdark/KD4 --dir .scratch/windows-evidence/<run-id>
```

The run page has a downloadable artifact named
`windows-validation-<tested-sha>-<run-id>-<attempt>`, retained for 14 days. Download
it promptly for a lasting audit record. GitHub's artifact API, including the
connected GitHub artifact tools, can retrieve the same ZIP. Read `summary.json`
first, then follow its relative evidence file paths. Logs retain the original
runner paths too; map `.../test-runner-logs/<name>` to `runner/<name>` inside the ZIP.

## Evidence and acceptance

`summary.json` uses schema `kd4_windows_validation_v1` and contains:

- Tested commit and expected event SHA, PR head/base SHAs, workflow revision,
  commit metadata, initial lockfile hash and clean-source checks.
- Windows image version, OS, architecture, CPU, memory/disk observations,
  Python/Rust/Cargo/nextest versions, MSVC installation details, and a narrow
  allowlist of build/test environment variables.
- Commands, working directories, UTC start/end times, durations, actual child
  exit codes, setup/download errors, and workflow step outcomes before upload.
- Expected test identities, runner-confirmed passes, unconfirmed tests, reporter
  failure observations, and the original structured execution receipt.
- Original runner ledgers with exact internal Cargo/nextest commands and timings,
  raw machine-readable test events, stdout/stderr, complete failure diagnostics,
  and provenance sidecars.
- Identity, size and SHA-256 of both helper executables and each generated test
  executable. These are post-execution observations in a fresh target directory;
  executable bytes are not uploaded and these are not compiler-read attestations.
- Relative names, sizes and SHA-256 hashes of every evidence file other than
  `summary.json` itself. The upload action supplies the archive's own digest.

Accept a result only when the job and upload succeeded, `status` is `passed`,
`problems` is empty, the source matches the candidate under review, and every
expected test has a confirmed receipt. Inspect the run's final job status as
well: `summary.json` is written **before** artifact upload and cannot attest that
upload or subsequent action cleanup succeeded. A null failure count means no
verified reporter outcomes were available, rather than zero failures.

The runner continues to own selection, ignored-test checks, zero-test rejection,
timeouts, failure semantics and execution receipts. The adapter checks these
against the selected manifest gates and retained logs. A compile-only success,
busy admission (exit 75), missing prerequisite, missing log/ledger/executable,
incomplete receipt, skipped setup/test step, or source/lockfile mutation cannot
produce a passing validation result. Process tests run with
`CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS=1` so unavailable prerequisites fail.

## Resources and boundaries

Rust follows `codex-rs/rust-toolchain.toml`; nextest is a pinned, SHA-256-verified
Windows release archive. The hosted image supplies Python >=3.11, PowerShell,
Visual C++ and the Windows SDK; setup/build failures remain visible if those
dependencies change. No extra Python packages, Just, sccache or benchmark stack
are needed. Direct public runner invocation is appropriate here because each
job has its own initially empty target directory and the runner reserves it.
Workstation validation should continue using the shared Just lanes.

Use the existing `dev-small` profile, two Cargo jobs, two nextest workers,
8 MiB stacks, no incremental output and no retries. Only Cargo source downloads
are cached, keyed by lockfile and toolchain; no executable or prior test result
is reused. The helper build uses `--locked`; the adapter also rejects lockfile
changes after runner execution. It preserves the repository's MSVC stack and
static CRT configuration without overriding `RUSTFLAGS`.

The overall job has a 60-minute bound, with bounded setup/build/test phases and
per-child runner timeouts. Retained logs have a 64 MiB per-file / 256 MiB total
bound; exceeding it retains a tail and explicitly fails evidence verification.
Do not upload build trees, home directories, environment dumps, credentials or
session data. `contents: read`, checkout without persisted credentials and
ordinary `pull_request` execution keep token permissions minimal.

Cancellation, runner loss or a job timeout may prevent finalization/upload.
Such runs are incomplete, never passes; use the surviving Actions logs and
rerun the exact intended revision when appropriate. A passing suite proves
these components compiled and these tests passed on the recorded Windows
environment. It establishes no end-to-end latency or wall-clock improvement.
