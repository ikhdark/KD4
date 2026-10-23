# Repository instructions

This file supplements the active agent instructions with KD4-specific facts, commands, and tool contracts shared by Codex and Claude; it does not replace their workflow or authorize broader task scope. Codex's general harness workflow lives in `codex-rs/protocol/src/prompts/base_instructions/default.md`; the local `CLAUDE.md` imports this file for Claude. Keep workflow rules out of this file and KD4-only content out of the harness prompt.

## Repository identity and runtime boundary

- This is the user's local fork of [`openai/codex`](https://github.com/openai/codex), for their own use; its main goal is to improve and optimize Codex. It is not intended for public release: upstream synchronization or distribution requires a request that explicitly names it.
- Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
- `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
- This repository contains the Rust CLI and app-server, not the native Windows shell.
- Other Codex and Claude sessions usually edit this checkout at the same time.

## Current source ownership

- `codex-rs/Cargo.toml` declares workspace membership. Follow current crate manifests and module declarations rather than assuming the old monolithic core layout. Session runtime is under `codex-rs/core/src/session/`; core integration tests are the separate `codex-core-integration-tests` package in `codex-rs/core/tests/`.
- Shared owners include `codex-rs/config/`, `codex-rs/features/`, `codex-rs/shell-command/`, `codex-rs/command-analysis/`, `codex-rs/sandboxing/`, `codex-rs/rollout/`, and `codex-rs/thread-store/`. Extend the existing owning crate rather than duplicating its implementation in core.
- `codex-rs/agent-graph-store/` owns thread-spawned agent topology; `codex-rs/agent-task-store/` owns durable MultiAgentV2 assignment state; `codex-rs/rollout-trace/` owns rollout trace schemas, writing, and replay reduction.
- `codex-rs/protocol/src/models.rs` embeds the canonical base prompt; `codex-rs/models-manager/src/prompt_resolver.rs` selects local policy, catalog, or fallback instructions. Reusable prompt templates live in `codex-rs/prompts/templates/`. Do not duplicate the base prompt in the model catalog or tool descriptions.
- Runtime tool handlers and registration live under `codex-rs/core/src/tools/`; shared tool types live in `codex-rs/tools/`. Code Mode is split across `codex-rs/code-mode/`, `codex-rs/code-mode-protocol/`, `codex-rs/code-mode-runtime/`, and `codex-rs/code-mode-host/`. Tool-specific instructions belong with their owning tool descriptions, not in the base prompt.
- V8 runs in the standalone `codex-code-mode-host` process, not the normal CLI/core dependency graph. `just deps-v8-boundary-check` checks that boundary without executing tests. Local publishing installs the CLI, Code Mode host, and Windows sandbox helpers as one runtime bundle.

## Python maintenance

- Python flags are per entrypoint: isolated `-I` execution hides sibling imports in some repository scripts. The invocations here carry the right flags (`-B` for `root_maintenance.py`, `-I -B` for the Rust runner).
- `scripts/maintenance/root_maintenance.py` owns Python test routing, including Harbor's separate pinned interpreter. From the repository root, use `python -B scripts/maintenance/root_maintenance.py test-python --module <module>` for a focused selection.
- Add `--json` to `test-python` for one structured result on stdout, with test diagnostics on stderr. Both presentations require nonempty explicit selections and complete worker reports; setup skips leaving selected tests unstarted are unverified. Both omit authored-test-body observations by default; add `--source-observations` to request fresh before/after hashes. Those hashes are observations, not dependency-aware validation receipts.
- `python -B scripts/maintenance/root_maintenance.py audit-scripts --quick --oracles` adds opt-in static Python assertion advisories to the script audit. The oracle worker does not execute analyzed sources; unresolved calls remain unknown, and `analysis_complete` is false. Oracle findings never become failures under `--strict` and are not coverage proof. `--quick` skips the full test suite, not the audit's other checks.

## Rust validation and generated schemas

- Prompt/documentation-only edits are covered by source/consumer review and focused policy checks; they do not require a Rust rebuild solely because text changed.
- `just config-schema-check` and `just app-server-schema-check --compatibility-baseline <rev>` check generated contracts without replacing fixtures; use them when those contracts change. The stable app-server baseline may instead be supplied through `CODEX_SCHEMA_COMPATIBILITY_BASELINE`; choose the intended contract revision rather than inventing one. Regeneration is explicit.
- Run these commands from the active repository root. `scripts/build/rust_test_runner.py` is the public runner; `codex-rs/.config/kd4-rust-tests.toml` owns target and gate names, selections, and helper binaries.
- The Rust runner and owned lane commands (`run-lane`, `cargo-lane.ps1 --prepare-sccache`) reuse an already-listening shared `sccache` server without spawning `--start-server`, and handle startup failures with one bounded, invocation-local fallback without the cache, before Cargo starts. Later runner phases keep that fallback; no shared server is restarted and no global environment is changed. Keep the recognized runner/Just invocation rather than wrapping it in arbitrary `python -c` code: those wrappers intentionally cannot authenticate runner receipts.
- Named gates and filtered targets:
  - `just check-lane <crate> --tests` compile-checks the selected package and its test targets without executing tests; select the separate integration-test package when its code changes.
  - `just core-test-gates-for <Rust-source-path> [...]` maps source paths to declared gates and target ownership.
  - `python -I -B scripts/build/rust_test_runner.py required-for <repo-relative-path> [...]` resolves explicitly declared consumer obligations without Cargo metadata, builds, or tests. This is separate from ownership advice; an empty result does not prove undeclared consumers are covered.
  - `just core-test-plan <target-or-gate>` previews the selection and helper builds without running tests.
  - `just core-gate <gate> [...]` runs related gates together, sharing helper builds and overlapping tests.
  - `just core-gate --changed-path <repo-relative-path> <gate> [...]` adds the required consumer gates for that path; repeat `--changed-path` for multiple paths when test execution is authorized.
  - `just core-test-fast <target> -E 'test(<test-name>)'` runs a focused target without retries.
  - `just core-test-list` lists target and gate names.
- Direct runner form: `python -I -B scripts/build/rust_test_runner.py run-target --profile fast core_lib -E 'test(<test-name>)'`. Replace angle-bracket placeholders. Put global options such as `--manifest`, `--cargo-profile`, and `--admission-timeout-seconds` before the subcommand; put execution options such as `--profile` before the target name, then filtering arguments after it. `--target-dir` accepts a caller-relative path; a managed lane such as `codex-rs/target/lanes/core-tests` names its lane family, so the runner reserves a free lane of that family as the `just` recipes do.
- Prefer the `just` recipes for execution: they reserve a lane of the shared `core-tests` family, preferring an idle warm lane with matching build settings and otherwise starting a new sibling (a cold build the first time). At most `CODEX_CARGO_LANE_MAX_ACTIVE_PER_BASE` (default 2) lanes of a family build at once, and maintenance keeps that many warm. Unfiltered `core_lib` requires explicit `--all`; zero-test selections fail rather than count as validation.
- A full Rust suite (`cargo test` or `cargo nextest run` without a test filter, or a target with `--all`) is admitted only as one standalone command with `--no-fail-fast` (nextest also `--retries 0`). A loop, pipeline or compound script is rejected; set the directory with the tool's `workdir` argument.
- Admission `busy` (exit 75, `codex_rust_admission_v1`) means every allowed lane is in use and required validation is **pending**, not failed or passed. It queues no background work, and the default wait is **0 seconds**, including `just` wrappers.
- The short creation/prune coordination lock is separate from a busy build: ordinary lane/runner admission allows a bounded **1-second** grace; a child inheriting an already-held lane retains its **30-second** handoff bound. Python `run-lane --lock-timeout-seconds` controls its coordination grace, not the busy-build wait.
- A busy record's `holder` names the runner invocation holding the target: its `argv`, working directory, start time and validation ledger path. `python -B scripts/build/rust_build_status.py lanes` reports the same for each active lane. `holder` is `null` when the holder is not a test runner (a plain Cargo command, for example). Check the holder for matching work before proposing another run.

## Codex performance and session analysis

- Use KDR when auditing or benchmarking Codex sessions.
- Compare matched Codex tasks by correct outcomes, total wall-clock time, input/output tokens, requests, tool calls, validation, retries, output recovery, and compactions. Separate model workflow choices from runtime changes; moving a wait into request normalization is not itself evidence of a slowdown. Microbenchmarks alone do not establish end-to-end gains.
- Rollout files (`.jsonl` or `.jsonl.zst`) live under the fork home's `sessions` directory (archived threads use `archived_sessions`); `rollout_payload_artifact` stub records resolve to the same home's `rollout-payloads/<sha256>.json`. Large records can live in these artifacts instead of inline rollout records; a stub is a reference, not the complete record.
- Put all audit documentation in `.codex/audits` under the active repository root.
- Audit/session-analysis scripts live in KDR. Live/build experiment launchers were retired; `scripts/benchmark_scripts` retains only compatibility helpers and non-audit regression fixtures required by KD4 tooling.
- The local KDR checkout is `C:\Users\kuh\Desktop\KDR`; use `kdr.cmd` there for saved-session analysis. `rolloutdetect.cmd` remains a compatibility entry point.
