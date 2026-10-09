# Repository instructions

KD4-specific context and commands; general agent workflow lives in `codex-rs/protocol/src/prompts/base_instructions/default.md`.

## Repository identity and runtime boundary

* This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Upstream synchronization or distribution requires a request that explicitly names it.
* This is a local project for the user's own use and is not intended for public release or distribution. Its main goal is to improve and optimize Codex.
* Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
* `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
* This repository contains the Rust CLI and app-server, not the native Windows shell.

## Python maintenance

* Do not apply one script's Python flags to every entrypoint. Use each owner's documented invocation; isolated `-I` execution can hide sibling imports in repository scripts. Follow the Rust runner invocation below for Rust validation.
* `scripts/root_maintenance.py` owns Python test routing, including Harbor's separate pinned interpreter. From the repository root, use `python -B scripts/root_maintenance.py test-python --module <module>` for a focused selection.
* Add `--json` to `test-python` for one structured result on stdout, with test diagnostics on stderr. This mode rejects zero-test selections; its authored-test-body hashes are observations, not dependency-aware validation receipts.
* `python -B scripts/root_maintenance.py audit-scripts --quick --oracles` adds opt-in static Python assertion advisories to the script audit. The oracle worker does not execute analyzed sources; unresolved calls remain unknown, and `analysis_complete` is false. Oracle findings never become failures under `--strict` and are not coverage proof. `--quick` skips the full test suite, not the audit's other checks.

## Rust validation and generated schemas

* Prompt/documentation-only edits normally need source/consumer review and focused policy checks, not a Rust rebuild. Performance work alone is not a reason to launch expensive Cargo validation; tie it to a specific unresolved correctness question.
* `just config-schema-check` and `just app-server-schema-check` check generated contracts without replacing fixtures; use them when those contracts change. Regeneration is explicit. Stable app-server schema checks require a chosen compatibility baseline, not an invented revision.

* Run these commands from the active repository root. `scripts/rust_test_runner.py` is the public runner; `codex-rs/.config/kd4-rust-tests.toml` owns target and gate names, selections, and helper binaries.
* Use the smallest applicable named gate or filtered target:
  * `just core-test-gates-for <Rust-source-path> [...]` maps source paths to declared gates and target ownership.
  * `just core-test-plan <target-or-gate>` previews the selection and helper builds without running tests.
  * `just core-gate <gate> [...]` runs related gates together, sharing helper builds and overlapping tests.
  * `just core-test-fast <target> -E 'test(<test-name>)'` runs a focused target without retries.
  * `just core-test-list` lists names only when the target or gate is not already known; do not precede known commands with repeated help/discovery calls.
* Direct runner form: `python -I -B scripts/rust_test_runner.py run-target --profile fast core_lib -E 'test(<test-name>)'`. Replace angle-bracket placeholders. Put global options such as `--manifest`, `--cargo-profile`, and `--admission-timeout-seconds` before the subcommand; put execution options such as `--profile` before the target name, then filtering arguments after it. `--target-dir` accepts a caller-relative path.
* Prefer the `just` recipes for execution: they reserve the shared warm `core-tests` Cargo lane. Sequence commands sharing a target directory; do not create cold lanes merely to parallelize validation. Unfiltered `core_lib` requires explicit `--all`; zero-test selections fail rather than count as validation.
* Admission `busy` (exit 75, `codex_rust_admission_v1`) means required validation is **pending**, not failed or passed. It queues no background work. The default wait is **0 seconds**, including `just` wrappers. Do not loop on fail-fast commands, switch lanes, or launch duplicates. Resume an already-live operation; otherwise, after edits settle and existing proof is checked, an explicit short wait is available: `just --set rust_validation_wait_seconds 5 core-gate <gate>`, or direct `--admission-timeout-seconds 5` / `run-lane --warm-wait-seconds 5`. Required validation does not require blocking this session. On busy or wait expiry, retain the obligation and report validation pending/blocked; do not mark the task fully validated, repeatedly check lane availability, or automatically retry.

## Codex performance and session analysis

* Compare matched Codex tasks by correct outcomes, total wall-clock time, input/output tokens, requests, tool calls, validation, retries, output recovery, and compactions. Separate model workflow choices from runtime changes; moving a wait into request normalization is not itself evidence of a slowdown. Microbenchmarks alone do not establish end-to-end gains.
* Analyze rollout JSONL (`LOCAL-KD\sessions`) in a script: count and hash bulk records (tool manifests, inventories, per-call timing arrays), compare timing aggregates, and emit only distinct, decision-relevant text. Complete scripted coverage satisfies a request to read logs fully; do not page every record into context.
