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

<!-- SHARED-OPERATING-POLICY: START -->
## Shared operating policy

### Scope and workspace

- Read every applicable `AGENTS.md` from the repository root through each path
  touched, and read every user-provided or user-named file in full.
- Work only within the requested scope. Do not broaden a directed fix or add
  unrelated cleanup, refactoring, dependency changes, or activation work.
- Do not publish, deploy, or modify upstream state unless the user explicitly
  requests that action.
- Preserve concurrent work and every unrelated hunk. Compare an overlapping
  target and its diff once, then keep or merge the version that satisfies every
  affected contract and direct test.
- Ask only when unresolved intent, incompatible user-visible outcomes, a
  required compatibility break, an unrequested destructive or external action,
  or conflicting validation criteria would materially change the solution.
  State the conflict and consequences; otherwise make the narrowest reasonable
  assumption, preserve existing behavior, and avoid over-engineering.

### Bug checks

- Report only defects supported by evidence. Do not report a design preference
  as a defect or change functional code merely because another design seems
  preferable.
- Each finding must identify the violated contract or invariant, responsible
  producer, reachable consumer or user-visible effect, and exact source
  locations. For a requested finding count, stop only after that many distinct
  findings meet all requirements; otherwise continue until every candidate in
  the requested scope is resolved.

### Implementation and validation

- For a code edit, add or update the direct test in the same change. The test
  must fail without the implementation change and pass with it.
- Produce schemas, snapshots, locks, vendored content, and other generated
  artifacts through their owner commands; do not hand-edit generated output.
- Run only the narrow tests that exercise the active implementation. Do not run
  broad test suites, repository-wide validation, or workspace analyzers unless
  the user requests that exact scope.
- A formatter, linter, build, applied patch, or successful command selecting
  zero relevant tests is not runtime proof. Runtime proof requires a direct
  contract test or a user-approved end-to-end gate that executes the changed
  path.

### Validation failure handling

- Run each scoped validation once initially. Retry transient infrastructure at
  most twice in the existing warmed build lane; do not create a cold lane
  solely to retry.
- Stop on unrelated or pre-existing compilation or test failures. Do not repair
  them without expanded scope; report passed direct checks, the blocker, and
  checks that could not run.
- Do not rerun validation while concurrent edits continue. After the same
  blocker occurs twice, finish and report it.
- Preserve narrower passing results despite broader failures.
<!-- SHARED-OPERATING-POLICY: END -->

## Routing and task scope

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
  above.
- After adding, deleting, moving, or renaming a repository file or directory,
  run `just source-map-check`. Run it even when ownership prose is unchanged;
  the command also rewrites the tracked-path snapshot.

## Delegated workflows

- Load [`.codex/harness/workflow.md`](.codex/harness/workflow.md) only when the
  request names delegation, a durable artifact, or the architect lane. Give
  each child only the role and rules assigned by that workflow. If a child fails
  to start, returns a tool error, or omits its assigned output, continue in the
  primary agent.

## Rust and script validation

- For a Rust edit, run the repository-named package or test filter that executes
  the changed contract. Validation passes only when at least one direct test is
  selected and the command exits successfully. Run workspace analyzers only
  when repository instructions or the user require them. If no direct route is
  named, report the missing route before editing.
- Do not run the full `codex-rs/core` test suite unless the user requests that
  exact scope.
- For a script edit, follow `scripts/AGENTS.md`. Run its named test; if none is
  named, run the sibling unit test. If none exists, run the interpreter syntax
  check and configured formatter or linter.

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
