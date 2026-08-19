## Project context

- This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Treat work as fork-local unless upstream or distribution work is requested.
- The published fork Desktop uses `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`;
  never substitute the profile `.codex` directory.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns repository inventory, runtime entrypoints,
  package and Rust-domain routing, the complete `codex-rs` edit and upstream-sync
  classification, generated contracts, validation routes, and cross-cutting
  change routes. Consult it before changing or upstream-syncing Rust workspace
  paths.
- After adding, deleting, moving, or renaming any repository file or directory,
  run `just source-map-check`. The command automatically rewrites the managed
  tracked-path snapshot in `SOURCEMAP.md`; this is required even when the
  ownership narrative and material inventories remain accurate.
- Read the nearest scoped `AGENTS.md`. `.codex/AGENTS.md` covers workspace
  routing; `.codex/config.toml` and `.codex/skills` own local config and skills.

## Desktop app boundary

- This repository has the Rust CLI and app-server, not the native Windows shell.
  Source changes require rebuilding/replacing the local binary and restarting
  Desktop before they are user-visible.

## Delegated and durable workflows

Load [`.codex/harness/workflow.md`](.codex/harness/workflow.md) only for
delegation, durable artifacts, or the architect lane. Give child agents only
their selected role and compact shared rules; fall back to default agents on
agent issues.

## Operating defaults

- Do not publish unless explicitly asked; stay within scope and do not broaden a
  directed fix.
- For bug checks, report only real defects, continue until none remain, and do
  not change functional code merely because another design seems better.
- Preserve concurrent work. Compare overlapping versions once and keep or merge
  the best compatible behavior.
- Ask when intent materially changes the solution; otherwise avoid
  over-engineering.
- Read any user-provided or user-named file in full.
- Do not run broad tests, do not run codex-rs/core full tests, only narrow tests on the active implementation is allowed unless told otherwise.


## Validation failure handling

- Run each scoped validation once initially. Retry transient infrastructure at
  most twice in the warmed Cargo lane; do not create a cold lane just to retry.
- Stop on unrelated/pre-existing compilation failures. Do not repair them
  without expanded scope; report passed local checks, the blocker, and checks
  that could not run.
- Do not rerun while concurrent edits continue. After the same blocker twice, finish.
- Preserve narrower passing results despite broad failures.

# Sessions

- Use `C:\Users\kuh\Desktop\LOCAL-KD\sessions` for fork rollouts, never
  `C:\Users\kuh\.codex\sessions`.
- For a live rollout, use `python scripts/rollout_snapshot.py <path> [--output
  <snapshot>]`. It opens the exact `.jsonl` path with shared access, reads the
  fixed length observed at open, and reports its SHA-256 identity.

## Validation

- Keep validation task-scoped; do not run full-suite/workspace tests unless asked.


# Benchmarking

- Do not label unit-test duration, test-binary startup time, analytical action-count projections, or source-level estimates as runtime benchmarks.
- "C:\Users\kuh\Desktop\LOCAL-KD\backups" contains previous fork binaries that can be used for A/B comparisons of the previous version to the current version.
- Optimize quality first, then latency and tokens. Define correctness, safety,
  fidelity, compatibility, and user-visible requirements; compare performance
  only among candidates passing the same quality contract.
- For an explicit optimization or a change to a known hot path, identify the
  repository's real workload, quality checks, latency metric and threshold, and
  token metric or budget before editing.
  Prefer an existing dedicated subprocess or end-to-end benchmark. If none
  exists, define a reproducible workload and compare equivalent current and
  candidate release builds; do not substitute a unit test or build duration.
- Prove quality before measuring. Treat latency/token misses as validation
  failures: inspect bounded owner work, fix it, re-prove quality, then rerun the
  same workload. Compare against the baseline/best correct candidate and reject
  regressions or threshold misses.
- Do not make a failing implementation pass by weakening an established
  workload, sample count, percentile, or threshold unless the user explicitly
  changes the performance contract. A benchmark rerun after a relevant code
  change is a new proof, not an unchanged-command retry under the validation
  failure rules above.
- Finish a performance-sensitive task only when the quality gate, latency
  contract, and token contract all pass, or report `partial` or `blocked` with
  the measured failure and remaining owner-level bottleneck. Report the actual
  workload, build identity, sample count, statistic, latency threshold, and
  token budget; do not generalize beyond the exercised path.
