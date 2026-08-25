# Scripts Policy

Keep background and usage examples in READMEs/help. Read nested policies in
`codex_package/` and `install/` when working there.

## Ownership

- Python owns maintenance, Cargo lane reservation, package staging, checks, mock
  servers, assembly, and script tests; PowerShell owns Windows publish lanes,
  compatibility wrappers, Rust perf setup, and install routing.
- `codex_package/` owns the canonical Codex package directory/archive builder for
  CLI and app-server artifacts.
- `install/` owns the Windows PowerShell install flow.
- `.venv/`, `__pycache__/`, and `*.pyc` are generated state, not source.

## Fast Routing

Read [`../SOURCEMAP.md`](../SOURCEMAP.md) only for ambiguous or cross-boundary
ownership.

- Local Codex binary publish/replacement proof: `publish-local-codex.ps1`,
  `test_publish_local_codex.py`.
- Rust lanes, target cleanup, and build diagnostics: `rust_build_status.py` is
  canonical; `cargo-lane.ps1` remains the direct no-Python and isolated-home
  adapter. Shared Python Rust environment policy lives in `rust_tool_env.py`;
  PowerShell adapters use `common-rust-env.ps1`, `invoke-rust-perf-env.ps1`, and
  `sccache-perf.ps1`.
- Package/npm staging: `build_codex_package.py`, `stage_npm_packages.py`,
  `codex_package/`.
- Install flow: `install/install.ps1`.
- Root maintenance lint, test, and audit commands: `root_maintenance.py`.
- Canonical formatting and repository checks: `format.py`, `asciicheck.py`, `readme_toc.py`,
  `source_map_check.py`, `check_blob_size.py`. Keep source-map rewriting
  deterministic and idempotent.
- PowerShell recipe invocation compatibility: `just-shell.py`.
- Tool versions: `tool_versions.py`; probes must not mutate or require network.
- Live rollout snapshots and analyzers: `rollout_snapshot.py` and
  `kd4_turn_latency_audit.py`; `kd4_first_useful_action_analysis.py` is its
  internal metric module. Read a growing rollout through a fixed-length,
  checksummed snapshot; do not stream an active JSONL file to unbounded EOF.

## High-Risk Surfaces

- Publish must preserve dry-run, backup, rollback, doctor, hash/version proof,
  process guards, and the `release` profile for `publish-local-codex-final`.
- Cargo lane scripts must preserve stop-parsing, argument forwarding, isolated
  target directories, and active-process checks before cleanup.
- Package staging must keep generated package layout aligned with
  `scripts/codex_package/`.
- Installers must preserve release/digest resolution, layout metadata, locking,
  PATH updates, migration, package-manager conflict checks, and shared behavior.
- Script wrappers such as `just-shell.py` treat quoting, argument forwarding,
  and exit-code propagation as compatibility surfaces.
- Mock-server and repository-check output/schema changes are caller contracts.

## Editing Rules

- Keep changes path-owned; do not mix publish, packaging, formatting, install,
  and maintenance behavior without a real dependency.
- For review, recommendation, agreement, reasons, or `what would you fix`
  requests, stay non-mutating unless the request explicitly asks for edits.
- Do not hand-edit generated files/locks, including `uv.lock`, or local caches.
- Preserve Windows behavior and use PowerShell/.NET primitives for platform
  integration.
- Prefer structured parsers and existing helper modules over ad hoc text
  manipulation for manifests, TOML, JSON, archives, and package metadata.
- Do not weaken publish safety guardrails unless explicitly requested.

## Validation

- If tests are waived, use focused syntax/read-back/dry-run/path checks and
  report the skip.
- For Python script changes, run the closest `python -m unittest
scripts.test_<name>` or package-local test when tests are not waived.
- For PowerShell, prefer parser checks and the closest dry-run/unit test. Do not
  close processes unless an authorized publish/update flow has passed its gates.
- For packaging changes under `codex_package/`, validate with focused package
  tests before broader packaging proof when tests are allowed.
- Desktop-visible publish changes require the root local publish/restart proof
  chain before claiming the running desktop app sees the change.

Report changed scripts, proof, skipped tests/platforms, and separate unrelated
dirty paths or blockers from task failures.
