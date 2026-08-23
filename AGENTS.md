# KD4 repository instructions

## Repository identity and runtime boundary

- This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Operate only on fork-local source and artifacts. Upstream synchronization or
  distribution requires a request that explicitly names it.
- Treat the active repository root as the checkout location; do not hard-code a
  workstation-specific checkout path.
- `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and
  `C:\Users\kuh\.codex` is the official upstream home. The published fork
  Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
- This repository contains the Rust CLI and app-server, not the native Windows
  shell. Source changes become Desktop-visible only after rebuilding and
  replacing or updating the local binary, then restarting Desktop. Perform
  those activation steps only when the request includes them.

## Routing and task scope

- Read every applicable `AGENTS.md` from the repository root through each path
  touched. Read every user-provided or user-named file in full.
- Before reading `SOURCEMAP.md` broadly, query the smallest named owner slice:
  `python scripts/source_owners.py slice --owner <owner-id> --focus "<task
  description>" --max-relationships 32`. Require an untruncated result with no
  omitted relationships or material unknowns, then read its exact evidence
  locations. Read the broad map only when no owner matches or the slice leaves
  an unresolved boundary.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns repository inventory, runtime entrypoints,
  package and Rust-domain routing, `codex-rs` edit and upstream-sync
  classification, generated contracts, validation routes, and cross-cutting
  change routes.
- Before editing, identify the source-map owner, direct callers and consumers,
  duplicate or generated representations, compatibility boundary, and named
  validation route. Record a source path or a scoped search with no match for
  each category.
- `.codex/AGENTS.md` covers workspace routing; `scripts/AGENTS.md` covers
  maintenance scripts; `.codex/config.toml` and `.codex/skills` own local
  configuration, fork-local skills, and validation workflows.
- Modify the requested behavior and the contract relationships identified
  above. Do not add unrelated cleanup, refactoring, dependency changes,
  publication, or activation.
- After adding, deleting, moving, or renaming a repository file or directory,
  run `just source-map-check`. Run it even when ownership prose is unchanged;
  the command also rewrites the tracked-path snapshot.
- Ask only when repository or tool output proves incompatible user-visible
  outcomes, a required compatibility break, an unrequested destructive or
  external action, or conflicting validation criteria. State the conflict and
  consequences. Otherwise preserve existing behavior.
- A bug finding must identify the violated contract or invariant, responsible
  producer, reachable consumer or user-visible effect, and exact source
  locations. For a requested finding count, stop when that many distinct
  findings meet all four requirements. For an exhaustive check, resolve every
  candidate in the mapped owner paths.

## Shared workspace and authorization

- Preserve concurrent work and every unrelated hunk. Compare overlapping
  versions once. Keep the version that satisfies every affected contract and
  direct test; merge non-conflicting required behavior. Ask under the conflict
  rule above only when the versions require incompatible user-visible behavior.
- Publish, deploy, or modify upstream state only when the request explicitly
  includes that action.

## Delegated workflows

- Load [`.codex/harness/workflow.md`](.codex/harness/workflow.md) only when the
  request names delegation, a durable artifact, or the architect lane. Give
  each child only the role and rules assigned by that workflow. If a child fails
  to start, returns a tool error, or omits its assigned output, continue in the
  primary agent.

## Implementation and validation

- For a code edit, add or update the direct test in the same change. The test
  must fail without the implementation change and pass with it.
- For a Rust edit, run the repository-named package or test filter that executes
  the changed contract. Validation passes only when at least one direct test is
  selected and the command exits successfully. Run workspace analyzers only
  when repository instructions or the user require them. If no direct route is
  named, report the missing route before editing.
- For a script edit, follow `scripts/AGENTS.md`. Run its named test; if none is
  named, run the sibling unit test. If none exists, run the interpreter syntax
  check and configured formatter or linter.
- Produce schemas, snapshots, locks, vendored content, and other generated
  artifacts through their owner commands; do not hand-edit generated files.
- A formatter, linter, build, or applied patch is not runtime proof. Runtime
  proof requires the direct contract test or a user-approved end-to-end gate
  that executes the changed path.
- Do not run broad tests.
- Whenever a full Cargo test or workspace test run is explicitly required or
  otherwise performed, use Cargo's `--no-fail-fast` mode and let the run finish
  before making fixes. Apply the same inventory-first rule to full-workspace
  Clippy, `cargo shear`, and other lint, analyzer, or quality-gate runs: complete
  the requested run and inventory every failing target, test, assertion, and
  diagnostic before implementation fixes begin.

## Sessions and rollout audits

- Use `C:\Users\kuh\Desktop\LOCAL-KD\sessions` for fork rollouts and
  `C:\Users\kuh\.codex\sessions` for official upstream rollouts.
- For a live rollout, use `python scripts/rollout_snapshot.py <path> [--output
  <snapshot>]`. It opens the exact `.jsonl` path with shared access, reads the
  fixed length observed at open, and reports its SHA-256 identity.
- For a session or turn latency audit, run
  `python scripts/kd4_turn_latency_audit.py <session-uuid-or-path>
  --sessions-root C:\Users\kuh\Desktop\LOCAL-KD\sessions --repo-root <repo>` as
  the only lookup and analysis pass. It resolves the exact UUID and performs the
  fixed-length snapshot internally; do not add file searches or ad hoc JSONL
  parsers before or after it.
- `audit decision: finalize` or `auditDecision.readyToFinalize=true` ends the
  audit. Answer from that report; continue only for blocker codes it lists.

## Benchmarking

- Before editing an explicit optimization or documented hot path, record the
  exact workload command, quality-gate command, latency statistic and threshold,
  and token metric and budget. If the request and repository provide no value
  for any field, ask for it and do not edit.
- Use the repository-named subprocess or end-to-end benchmark. If none is
  named, request the workload command, inputs, build profile, environment,
  sample count, and statistic. Hold them constant for baseline and candidate
  release builds.
- Unit-test duration, test-binary startup, build duration, analytical action
  counts, and source estimates are not runtime benchmarks.
- For an A/B comparison with a previous fork version, use binaries in
  `C:\Users\kuh\Desktop\LOCAL-KD\backups` as the baseline.
- Run the quality gate before each comparison. Do not benchmark a failing
  candidate. Reject a candidate that exceeds a recorded threshold or regresses
  against the passing baseline.
- Do not weaken an established workload, sample count, percentile, latency
  threshold, or token budget unless the user explicitly changes the contract.
- After changing the workload implementation, inputs, build, or measured path,
  rerun the quality gate and then the unchanged workload. This is a new proof,
  not an unchanged-command retry.
- Finish only when the quality gate passes and measured latency and token use
  stay within their recorded limits. If code changed and a limit still fails,
  report `partial`. Report `blocked` only when a required input, permission, or
  external failure prevents another authorized change. Report the workload,
  build identity, sample count, statistic and measured value, latency threshold,
  token measurement, and token budget. Limit claims to that workload.
