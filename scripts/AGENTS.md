# Scripts Policy

This file applies inside `scripts/`. It inherits the repository-wide policy from
the root `AGENTS.md`.

Keep this file focused on script editing, ownership, and validation. Put long
background or usage examples in README files or script help text.

## Scoped Policy

Known scoped instruction files include:

- `codex_package/AGENTS.md`: canonical package directory/archive builder for CLI
  and app-server artifacts.
- `install/AGENTS.md`: standalone shell and PowerShell installer entrypoints.

Do not assume other nested `AGENTS.md` files exist unless they are present in the
working tree.

## Ownership

- Python utilities own repository maintenance, package staging, README/table
  checks, blob-size checks, mock Responses WebSocket serving, local package
  assembly, script test coverage, and tooling metadata.
- PowerShell utilities own Windows local-build lanes, local publish flows,
  PowerShell invocation wrappers, Rust performance environment setup, install
  routing, and target cleanup helpers.
- Shell utilities own Unix install, debug, remote-environment, Bazel target
  listing, and helper launch paths.
- `codex_package/` owns the canonical Codex package directory/archive builder for
  CLI and app-server artifacts.
- `install/` owns platform install entrypoints for shell and PowerShell install
  flows.
- `.venv/`, `__pycache__/`, and `*.pyc` are local generated state, not source.

## Fast Routing

Use this section first. Read [`../SOURCEMAP.md`](../SOURCEMAP.md) only when the
owner crosses script/package boundaries or remains ambiguous.

- Local Codex binary publish/replacement proof: `publish-local-codex.ps1`,
  `publish-local-codex.hashing.ps1`, `test_publish_local_codex.py`.
- Rust lanes, target cleanup, and build diagnostics: `cargo-lane.ps1`,
  `cargo-lane-trash-cleanup.ps1`, `rust_build_status.py`, `common-rust-env.ps1`,
  `invoke-rust-perf-env.ps1`, `sccache-perf.ps1`.
- Package assembly and npm staging: `build_codex_package.py`,
  `stage_npm_packages.py`, `codex_package/`; read
  `codex_package/AGENTS.md` for package-contract work.
- Platform install flows: `install/install.sh`, `install/install.ps1`; read
  `install/AGENTS.md` for installer-contract work.
- Root maintenance commands: `root_maintenance.py`, synchronized with root
  `package.json` script names.
- Repository checks: `format.py`, `asciicheck.py`, `readme_toc.py`,
  `check_blob_size.py`, and matching tests.
- PowerShell/script invocation compatibility: `just-shell.py`,
  `run-powershell-script.ps1`, `test_run_powershell_script.py`.
- Tool-version reporting: `tool_versions.py`; probes must not mutate the machine
  or require network access.

## High-Risk Surfaces

- Publish scripts must preserve dry-run, backup, rollback, doctor,
  hash/version proof, process detection, process-closing protections, and the
  optimized `release` profile for final `just publish-local-codex-final` proof.
  Use `local-release` and `-BuildOnly` only to warm or iterate without replacing
  the installed binary.
- Cargo lane scripts must preserve stop-parsing, argument forwarding, isolated
  target directories, and active-process checks before cleanup.
- Package staging must keep generated package layout aligned with
  `scripts/codex_package/`.
- Install scripts must preserve release resolution, digest verification,
  standalone layout metadata, install locking, PATH updates, old-install
  migration, and conflicting package-manager detection. Keep shell and
  PowerShell installer behavior aligned where they share a contract.
- Script wrappers such as `just-shell.py` and `run-powershell-script.ps1` treat
  quoting, argument forwarding, and exit-code propagation as compatibility
  surfaces.
- Mock websocket server, Bazel helper, and repository-check
  output/schema changes are contract changes for their callers/tests.

## Editing Rules

- Keep script changes narrow and path-owned. Do not mix publish tooling, package
  staging, formatter, install, and maintenance behavior unless one change truly
  requires the other.
- For review, recommendation, agreement, reasons, or `what would you fix`
  requests, stay non-mutating unless the request explicitly asks for edits.
- Do not hand-edit generated files or locks. Treat `uv.lock` as generated
  dependency metadata; leave `.venv/`, `__pycache__/`, and `*.pyc` as regenerated
  local state.
- Preserve cross-platform behavior. If a script has PowerShell and shell/Python
  siblings, check whether the same behavior needs a matching update.
- Prefer structured parsers and existing helper modules over ad hoc text
  manipulation for manifests, TOML, JSON, archives, and package metadata.
- For local publish tooling, do not weaken backup, rollback, dry-run, doctor, or
  running-process guardrails unless the request explicitly targets that safety
  surface.

## Validation

- If the request says no tests, do not run test commands. Use focused non-test
  checks such as syntax checks, read-back proof, dry-run proof, or command/path
  existence checks, and report skipped tests.
- For Python script changes, run the closest `python -m unittest
  scripts.test_<name>` or package-local test when tests are not waived.
- For PowerShell scripts, prefer parse/syntax checks and the closest dry-run or
  unit test. Do not run process-closing publish commands unless the task is a
  publish/update flow and selected proof gates have passed.
- For packaging changes under `codex_package/`, validate with focused package
  tests before broader packaging proof when tests are allowed.
- Desktop-visible publish changes require the root local publish/restart proof
  chain before claiming the running desktop app sees the change.

## Reporting

- Report changed scripts, selected proof, skipped tests, and any platform path not
  validated.
- Separate unrelated dirty paths or pre-existing tooling blockers from task-owned
  script failures.
