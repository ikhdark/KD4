# Scripts

Agent workflow rules for this directory live in `scripts/AGENTS.md`. This README
is the current source map for the checked-in script tooling.

## Ownership

- Python utilities own repository maintenance, package staging, README/table
  checks, blob-size checks, mock Responses WebSocket serving, local package
  assembly, script test coverage, and tooling metadata.
- PowerShell utilities own Windows local-build lanes, local publish flows,
  PowerShell invocation, Rust performance environment setup, install routing,
  and target cleanup helpers.
- Shell utilities own Unix install, debug, remote-environment, Bazel target
  listing, and helper launch paths.
- `scripts/codex_package/` owns the canonical Codex package directory/archive
  builder for CLI and app-server artifacts.
- `scripts/install/` owns platform install entrypoints for shell and PowerShell
  install flows.
- `scripts/.venv/`, `scripts/__pycache__/`, and `*.pyc` are local generated
  state when present. They do not own source behavior.

## Routing

- Local Codex runtime-bundle replacement and desktop/CLI publish proof:
  `publish-local-codex.ps1`, `publish-local-codex.hashing.ps1`, and
  `test_publish_local_codex.py`.
- Rust build lanes, target cleanup, and build-status diagnostics:
  `cargo-lane.ps1`, `cargo-lane-trash-cleanup.ps1`, `rust_build_status.py`,
  `common-rust-env.ps1`, `invoke-rust-perf-env.ps1`, and `sccache-perf.ps1`.
- Package assembly and npm/package staging: `build_codex_package.py`,
  `stage_npm_packages.py`, and `codex_package/`.
- Platform install flows: `install/install.sh` and `install/install.ps1`.
- Root package maintenance commands: `root_maintenance.py`. The root
  `package.json` `format:python*`, `lint:python*`, and `test:scripts*` aliases
  route Python script maintenance through this helper.
- Repository checks: `format.py`, `asciicheck.py`, `readme_toc.py`,
  `check_blob_size.py`, and their matching tests.
- Script invocation compatibility: `just-shell.py`, `run-powershell-script.ps1`,
  and `test_run_powershell_script.py`.
- Tool-version reporting: `tool_versions.py`.
- Generated local Python state: `.venv/`, `__pycache__/`, and `*.pyc`.

## Script Context

- `publish-local-codex.ps1`: Windows local runtime-bundle publish flow. It builds
  and publishes `codex.exe` plus `codex-code-mode-host.exe` beside each other.
  Preserve dry-run, per-artifact backup, bundle rollback, doctor, hash/version
  proof, process detection, and process-closing protections. Final local publish
  proof should stay on the optimized `release` profile via
  `just publish-local-codex-final`; use `local-release` and `-BuildOnly` only to
  warm or iterate on the exact publish target without replacing the installed
  binaries.
- `publish-local-codex.hashing.ps1`: hashing/version helper code for publish
  proof. Keep output contracts stable for publish tests and doctor reporting.
- `test_publish_local_codex.py`: focused coverage for local publish behavior.
- `cargo-lane.ps1`: isolated Cargo/just lane runner for concurrent Rust work.
  Preserve stop-parsing and argument-forwarding behavior.
- `cargo-lane-trash-cleanup.ps1`: cleanup for lane trash/target artifacts. Never
  remove active build outputs without active-process checks.
- `common-rust-env.ps1`: shared Rust build environment setup for Windows scripts.
  Publish and lane scripts should pass Cargo target directories with
  `--target-dir` instead of exporting `CARGO_TARGET_DIR`, so sccache keys stay
  reusable across repeated local builds.
- `rust_build_status.py`: reports active Rust/build processes and target state.
- `rust_packages.py`: Rust package/workspace metadata helper. Prefer Cargo
  metadata over hand-maintained crate lists.
- `invoke-rust-perf-env.ps1` and `sccache-perf.ps1`: local Rust performance and
  cache diagnostics. Do not change cache topology without measured need.
- `build_codex_package.py`, `stage_npm_packages.py`, and
  `test_stage_npm_packages.py`: package assembly/staging. Keep generated package
  layout aligned with `scripts/codex_package/`.
- `root_maintenance.py`: repository root maintenance helper for Python script
  formatting, linting, dead-code checks, and script tests. Keep target lists
  explicit and synchronized with root `package.json` script names.
- `format.py`: repository formatting entry point. Keep rustfmt/toolchain behavior
  aligned with `codex-rs/rustfmt.toml` and `codex-rs/rust-toolchain.toml`.
- `asciicheck.py`, `check_blob_size.py`, `readme_toc.py`,
  and matching `test_*` files: focused repository checks. Prefer updating the
  check and its focused test together when behavior changes.
- `reasoning_quality_eval.py` and `test_reasoning_quality_eval.py`: reasoning
  evaluation helper. Treat output/schema changes as evaluator contract changes.
- `mock_responses_websocket_server.py` and
  `test_mock_responses_websocket_server.py`: mock Responses WebSocket server for
  local protocol/testing flows.
- `run_tui_with_exec_server.sh`, `start-codex-exec.sh`,
  `test_run_tui_with_exec_server.py`, and `debug-codex.sh`: local launch/debug
  helpers. Preserve cross-platform path assumptions and environment forwarding.
- `just-shell.py`: just shell wrapper. Treat quoting/argument forwarding as the
  primary compatibility surface.
- `list-bazel-clippy-targets.sh`, `list-bazel-release-targets.sh`, and
  `check-module-bazel-lock.sh`: Bazel target/lock helpers. Keep output parsable
  for CI or justfile callers.
- `tool_versions.py`: centralized tool-version reporting. Avoid network or
  machine mutation in version probes.
- `pyproject.toml`: Python script tooling metadata.
- `uv.lock`: generated Python dependency lock metadata. Do not hand-edit.
- `.venv/`: ignored local Python virtual environment. Recreate it from the
  checked-in script tooling metadata instead of editing installed packages,
  activation scripts, or interpreter state.
- `__pycache__/` and `*.pyc`: ignored Python bytecode caches. Do not treat cache
  timestamps or compiled bytecode as source evidence.
- `install/install.sh` and `install/install.ps1`: platform install entrypoints.
  Preserve release resolution, digest verification, standalone layout metadata,
  install locking, PATH updates, old-install migration, and conflicting package
  manager detection. Keep shell and PowerShell behavior aligned where both
  platforms implement the same installer contract.
- `run-powershell-script.ps1` and `test_run_powershell_script.py`: PowerShell
  script invocation wrapper and focused validation.
- `test_build_tooling.py`, `test_cargo_lane.py`, `test_check_blob_size.py`,
  `test_readme_toc.py`, `test_run_tui_with_exec_server.py`, and
  `test-remote-env.sh`: focused validation for the named script/tooling surface.

## Editing Rules

- Keep script changes narrow and path-owned. Do not mix publish tooling, package
  staging, formatter, install, and maintenance behavior unless one change truly
  requires the other.
- For review, recommendation, agreement, reasons, or `what would you fix`
  requests, stay non-mutating unless the request explicitly asks for edits.
- Do not hand-edit generated files or locks. In particular, treat `uv.lock` as
  generated dependency metadata, and leave `.venv/`, `__pycache__/`, and `*.pyc`
  as regenerated local state.
- Preserve cross-platform behavior. If a script has PowerShell and shell/Python
  siblings, check whether the same behavior needs a matching update.
- Prefer structured parsers and existing helper modules over ad hoc text
  manipulation for manifests, TOML, JSON, archives, and package metadata.
- For local publish tooling, preserve backup, rollback, dry-run, doctor, and
  running-process guardrails unless the request explicitly targets that safety
  surface.

## Validation

- If the request says no tests, do not run test commands. Use focused non-test
  checks only when relevant, such as syntax checks, read-back proof, dry-run
  proof, or command/path existence checks, and report skipped tests. Do not
  suggest test gates in that turn.
- Use the local command summarizer for supported high-output commands such as
  `git status`, `git diff`, `just`, `cargo`, `pnpm`, and `npm` when available
  and the command fits its supported invocation shape. Keep exact searches raw
  and bounded.
- For Python script changes, run the closest `python -m unittest
  scripts.test_<name>` or package-local test only when tests are not waived.
- For PowerShell scripts, prefer parse/syntax checks and the closest dry-run or
  unit test. Do not run process-closing publish commands unless the task is a
  publish/update flow and the selected proof gates have passed.
- For packaging changes under `scripts/codex_package/`, validate with the
  focused package tests before broader packaging proof when tests are allowed.
- Desktop-visible publish changes require the root local publish/restart proof
  chain before claiming the running desktop app sees the change.

## Reporting

- Report changed scripts, selected proof, skipped tests, and any platform path
  not validated.
- Separate unrelated dirty paths or pre-existing tooling blockers from
  task-owned script failures.
