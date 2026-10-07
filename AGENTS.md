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

## Rust runner quick reference

Run these from the checkout root; use the manifest-owned runner instead of guessing Cargo targets.

* `python scripts/rust_test_runner.py list-targets` lists target names.
* `python scripts/rust_test_runner.py gates-for <repository-relative-file> ...` reports declared target/gate ownership without running tests.
* `python scripts/rust_test_runner.py plan <target-or-gate>` previews commands and helper requirements; it does not run tests.
* `python scripts/rust_test_runner.py run-target <name> -- -E 'test(module::test_name)'` runs a filtered nextest selection; use `test(=fully::qualified::test)` for an exact test. Unfiltered `core_lib` requires explicit `--all`.
* Runner-wide `--target-dir <path>`, `--cargo-profile <profile>`, and `--admission-timeout-seconds <seconds>` go before the subcommand. Do not run simultaneous Cargo commands against the same target directory.
* `python scripts/rust_build_status.py doctor` reports build health/contenders; `lanes` lists active/stale target lanes; `disk` reports target disk usage. These inspect rather than build or clean.

## Session logs

* Analyze rollout JSONL (`LOCAL-KD\sessions`) in a script: count and hash bulk records (tool manifests, inventories, per-call timing arrays), compare timing aggregates, and emit only distinct, decision-relevant text. Complete scripted coverage satisfies a request to read logs fully; do not page every record into context.


## Harness semantics and wall-clock performance

* Prefer replacing an existing round trip over adding any new persistent behavior.
* Do not add infrastructure, including reasoning infrastructure, unless it demonstrably removes more critical-path work than it introduces.
* A faster tool can encourage a slower overall workflow. Judge KD4 harness changes by end-to-end time to a correct, complete outcome, not isolated operation latency or token savings.
* Compare total task wall-clock, model requests, tool calls, validation time, retries/recovery, and correctness/completeness. A local speedup does not justify additional calls or an end-to-end wall-clock regression; do not trade away correctness or completeness for speed.
* Do not introduce mandatory reasoning ledgers or automatic behavioral machinery merely to improve semantic assistance. They can create new reasons for the model to continue working; require demonstrated net critical-path savings before adding them.
* Prefer changes that replace work the agent already performs. New semantic behavior should have a demonstrated repeated failure or round trip it eliminates, not merely a plausible reasoning benefit.