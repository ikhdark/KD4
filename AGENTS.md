# Repository instructions

## Repository identity and runtime boundary

* This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Upstream synchronization or distribution requires a request that explicitly names it.
* This is a local project for the user's own use and is not intended for public release or distribution. Its main goal is to improve and optimize Codex.
* Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
* `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
* This repository contains the Rust CLI and app-server, not the native Windows shell.

## Scope and workspace

* Prefer existing owners and abstractions; identify a verified missing capability before adding machinery. Preserve overlapping user work and verify the combined runtime path end to end.

## Tooling boundaries

* Do not apply one script's Python flags to every entrypoint. Use each owner's documented invocation; isolated `-I` execution can hide sibling imports in repository scripts. Follow the Rust runner invocation below for Rust validation.

## Validation cost and priorities

* Prioritize finding and fixing meaningful problems over routinely recompiling Rust after small changes. Use the smallest verification sufficient for the affected behavior and risk.
* Reuse existing test results when they remain applicable to the current source and dependencies. Prefer source-level verification and lightweight, targeted checks; do not treat stale results as current proof.
* Run compilation or tests only when necessary to establish correctness. Do not launch expensive Cargo builds or test suites merely to validate a performance improvement; identify the specific correctness question that cheaper evidence cannot resolve before starting them.
* Avoid competing with existing builds or duplicating live validation. Reuse relevant results and the shared warm build lane rather than starting another build for reassurance.
* State what was verified, what was not, and any remaining uncertainty. Do not claim unrun checks passed or unmeasured performance gains were established.
* For prompt/documentation-only edits, prefer source/consumer review and focused policy checks over a Rust rebuild. Use the generation owners only when their contracts change: `just config-schema-check` and `just app-server-schema-check` check without replacing fixtures; regeneration is explicit. Stable app-server schema checks require a chosen compatibility baseline, not an invented revision.

## Rust runner quick reference

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

## Harness semantics and wall-clock performance

* For Rust CLI/app-server harness changes, preserve retained-output completeness, continuation/recovery, evidence freshness, and live-operation lifecycle contracts through their real consumers. Follow the active tool contracts and base instructions rather than copying generic harness rules into this file.
* Keep default bounded tool displays unless the next decision requires more source. Retain complete bulk evidence before projecting it; recover only missing ranges instead of rerunning producers or requesting oversized output by default. Smaller displays must not reduce required audit coverage.
* Compare matched tasks by correct outcomes, total wall-clock time, input/output tokens, requests, tool calls, validation, retries, output recovery, and compactions. Separate model workflow choices from runtime changes; moving a wait into request normalization is not itself evidence of a slowdown. Report unmeasured phases and do not claim end-to-end gains from passing tests or microbenchmarks alone.

## Session logs

* Analyze rollout JSONL (`LOCAL-KD\sessions`) in a script: count and hash bulk records (tool manifests, inventories, per-call timing arrays), compare timing aggregates, and emit only distinct, decision-relevant text. Complete scripted coverage satisfies a request to read logs fully; do not page every record into context.
