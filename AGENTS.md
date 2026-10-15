# Repository instructions

This file supplements the active agent instructions with KD4-specific facts, commands, and tool contracts shared by Codex and Claude; it does not replace their workflow or authorize broader task scope. General workflow lives with each agent: Codex's harness prompt source is `codex-rs/protocol/src/prompts/base_instructions/default.md`; Claude's is the local `CLAUDE.md`. Keep workflow rules out of this file and KD4-only content out of the harness prompt.

## Repository identity and runtime boundary

* This is the user's local fork of [`openai/codex`](https://github.com/openai/codex), for their own use; its main goal is to improve and optimize Codex. It is not intended for public release: upstream synchronization or distribution requires a request that explicitly names it.
* Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
* `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
* This repository contains the Rust CLI and app-server, not the native Windows shell.
* Other Codex and Claude sessions usually edit this checkout at the same time.

## Python maintenance

* Python flags are per entrypoint: isolated `-I` execution hides sibling imports in some repository scripts. The invocations here carry the right flags (`-B` for `root_maintenance.py`, `-I -B` for the Rust runner).
* `scripts/maintenance/root_maintenance.py` owns Python test routing, including Harbor's separate pinned interpreter. From the repository root, use `python -B scripts/maintenance/root_maintenance.py test-python --module <module>` for a focused selection.
* Add `--json` to `test-python` for one structured result on stdout, with test diagnostics on stderr. Both presentations require nonempty explicit selections and complete worker reports; setup skips leaving selected tests unstarted are unverified. JSON authored-test-body hashes are observations, not dependency-aware validation receipts.
* `python -B scripts/maintenance/root_maintenance.py audit-scripts --quick --oracles` adds opt-in static Python assertion advisories to the script audit. The oracle worker does not execute analyzed sources; unresolved calls remain unknown, and `analysis_complete` is false. Oracle findings never become failures under `--strict` and are not coverage proof. `--quick` skips the full test suite, not the audit's other checks.

## Rust validation and generated schemas

* Prompt/documentation-only edits are covered by source/consumer review and focused policy checks; they do not require a Rust rebuild solely because text changed.
* `just config-schema-check` and `just app-server-schema-check --compatibility-baseline <rev>` check generated contracts without replacing fixtures; use them when those contracts change. The stable app-server baseline may instead be supplied through `CODEX_SCHEMA_COMPATIBILITY_BASELINE`; choose the intended contract revision rather than inventing one. Regeneration is explicit.
* Run these commands from the active repository root. `scripts/build/rust_test_runner.py` is the public runner; `codex-rs/.config/kd4-rust-tests.toml` owns target and gate names, selections, and helper binaries.
* Named gates and filtered targets:
  * `just core-test-gates-for <Rust-source-path> [...]` maps source paths to declared gates and target ownership.
  * `just core-test-plan <target-or-gate>` previews the selection and helper builds without running tests.
  * `just core-gate <gate> [...]` runs related gates together, sharing helper builds and overlapping tests.
  * `just core-test-fast <target> -E 'test(<test-name>)'` runs a focused target without retries.
  * `just core-test-list` lists target and gate names.
* Direct runner form: `python -I -B scripts/build/rust_test_runner.py run-target --profile fast core_lib -E 'test(<test-name>)'`. Replace angle-bracket placeholders. Put global options such as `--manifest`, `--cargo-profile`, and `--admission-timeout-seconds` before the subcommand; put execution options such as `--profile` before the target name, then filtering arguments after it. `--target-dir` accepts a caller-relative path.
* Prefer the `just` recipes for execution: they reserve the shared `core-tests` Cargo lane family and can reuse an idle warm sibling with matching build settings. Do not create cold lanes merely to parallelize validation. Unfiltered `core_lib` requires explicit `--all`; zero-test selections fail rather than count as validation.
* A full Rust suite (`cargo test` or `cargo nextest run` without a test filter, or a target with `--all`) is admitted only as one standalone command with `--no-fail-fast` (nextest also `--retries 0`). A loop, pipeline or compound script is rejected; set the directory with the tool's `workdir` argument.
* Admission `busy` (exit 75, `codex_rust_admission_v1`) means required validation is **pending**, not failed or passed. It queues no background work, and the default wait is **0 seconds**, including `just` wrappers.
* The short creation/prune coordination lock is separate from a busy build: ordinary lane/runner admission allows a bounded **1-second** grace; a child inheriting an already-held lane retains its **30-second** handoff bound. Python `run-lane --lock-timeout-seconds` controls its coordination grace, not the busy-build wait.
* A busy record's `holder` names the runner invocation holding the target: its `argv`, working directory, start time and validation ledger path. `python -B scripts/build/rust_build_status.py lanes` reports the same for each active lane. `holder` is `null` when the holder is not a test runner (a plain Cargo command, for example). Check the holder for matching work before proposing another run.

## Codex performance and session analysis

* Compare matched Codex tasks by correct outcomes, total wall-clock time, input/output tokens, requests, tool calls, validation, retries, output recovery, and compactions. Separate model workflow choices from runtime changes; moving a wait into request normalization is not itself evidence of a slowdown. Microbenchmarks alone do not establish end-to-end gains.
* Rollout JSONL lives under the fork home's `sessions` directory (archived threads use `archived_sessions`); `rollout_payload_artifact` stub records resolve to the same home's `rollout-payloads/<sha256>.json`. Large records can live in these artifacts instead of inline JSONL; a stub is a reference, not the complete record.
* Put all audit documentation in `.codex/audits` under the active repository root.
* Put all benchmark/audit scripts in `scripts/benchmark_scripts` under the active repository root.
* "C:\Users\kuh\Documents\ChatGPT\rolloutdetect" provides extra tools for auditing and benchmarking.