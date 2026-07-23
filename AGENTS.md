# Repository policy

Shared policy revision: `2026-07-22`.

## Synchronization contract

The canonical shared-policy source is:

- `C:\Users\kuh\Desktop\kd4\AGENTS.md`

Its synchronized targets are:

- `C:\Users\kuh\Desktop\KDWG\AGENTS.md`
- `C:\Users\kuh\Desktop\kds-main\AGENTS.md`
- `C:\Users\kuh\Desktop\mdpwa-main\AGENTS.md`
- `C:\Users\kuh\Desktop\kdsb-main\AGENTS.md`
- `C:\Users\kuh\Desktop\kdpc-main\AGENTS.md`
- `C:\Users\kuh\Desktop\kdgma-main\AGENTS.md`

Every byte outside the project-context block below must remain identical across
all seven files. Only that block may contain repository-specific identity,
ownership, commands, validation, runtime, installation, safety, or protected
path details.

Change shared rules only in the canonical KD4 source. When a shared rule
changes, update its revision, copy the shared portion to all six targets in the
same task, and compare normalized copies after replacing each project-context
block with the same sentinel. Do not place repository-specific exceptions
outside that block or edit a target's shared portion independently.

## Project context

<!-- PROJECT-CONTEXT: START (only this block may differ) -->
### Identity and objective

- This checkout is the user's local fork of
  [`openai/codex`](https://github.com/openai/codex) at
  `C:\Users\kuh\Desktop\kd4`. Its home repository is
  [`ikhdark/KD4`](https://github.com/ikhdark/KD4).
- KD4 is a modified Codex for the user's improvements, experiments, local
  workflows, and fork-specific tooling. Treat work as fork-local unless the
  user explicitly requests upstream, product-facing, or distribution-ready
  changes.
- The standing objective is to improve, audit, and optimize the checkout while
  keeping changes reviewable, local-build focused, and easy to validate.

### Source of truth and ownership

- Known top-level instruction scopes include `.codex/AGENTS.md`,
  `codex-rs/AGENTS.md`, and `scripts/AGENTS.md`; further nested files apply
  only where present.
- `.codex/AGENTS.md` and `.codex/README.md` own workspace policy and routing.
  `.codex/config.toml` owns optional repo-local runtime configuration,
  `.codex/environments` owns worktree setup/state, and `.codex/skills` owns
  fork-local skills and validation workflows.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns the high-level directory map, runtime
  entrypoints, Rust-domain routing, build/package/publish paths, generated
  contracts, and cross-cutting change routes.

### Project constraints

- Preserve upstream-compatible behavior unless the user explicitly wants
  local-only fork behavior. Call out changes affecting public CLI flags,
  app-server APIs, configuration loading, sandbox behavior, stored sessions,
  rollout compatibility, or installed-binary behavior.
- Identify the owning contract before editing protocol, app-server, SDK,
  configuration-schema, generated-artifact, or publish-path behavior. Update
  generated outputs only through the owning generator or recipe.
- The repository contains the Rust CLI and app-server components used by Codex
  Desktop, but not the native Windows desktop shell source. Source edits do not
  hot-apply to the installed app.

### Validation and runtime proof

- Rust changes: work from `codex-rs` and prefer the focused crate `just` recipe
  or focused Cargo check/test.
- App-server schema or protocol: run focused app-server tests and
  `just app-server-schema-check`. Use the force or raw generator recipes only
  for intentional contract regeneration.
- Configuration schema: run focused config/core tests and
  `just config-schema-check`. Use force or raw generator recipes only for
  intentional `codex-rs/core/config.schema.json` regeneration.
- Python SDK changes: use focused `uv run pytest` and `uv run ruff check .`;
  regenerate locks or artifacts only for touched SDK surfaces.
- Script changes: run syntax checks and the closest script tests. Do not
  hand-edit generated locks such as `scripts/uv.lock`.
- Root maintenance: prefer matching root `package.json` scripts.
- Local publish path changes: use `just publish-local-codex-dry-run` for path
  proof and `just publish-local-codex-final` before claiming installed
  replacement.
- For a local-build claim, prove only the applicable links: relevant crates
  compile, focused tests pass, the local `codex` binary builds, publish or
  dry-run paths succeed when touched, installed replacement is correct when
  touched, and the active desktop/app-server uses the local build after
  rebuild, publish, and restart.
- `CODEX_LOCAL_PUBLISH_DIR` is `C:\Users\kuh\Desktop\LOCAL-KD`, so the expected
  publish target is `C:\Users\kuh\Desktop\LOCAL-KD\codex.exe`. Desktop-visible
  completion also requires the running process path, local binary hash/version,
  relevant app-server initialize/model metadata, and a user-visible screenshot
  or equivalent evidence. State whether `just publish-local-codex-final` and a
  Desktop restart remain required.

### Protected paths and state

- Do not hand-edit `codex-rs/target`, `node_modules`, `codex-rs/vendor`,
  `third_party`, or `codex-rs/app-server-protocol/schema`.
<!-- PROJECT-CONTEXT: END -->

## Instruction scope

This file applies repository-wide.

Before editing, locate applicable instructions with:

```text
rg --files --hidden -g AGENTS.md
```

Read the closest relevant `AGENTS.md`. A nearer file augments this policy and
takes precedence inside its subtree. Do not rely on instruction files absent
from the working tree.

Keep shared rules in the synchronized root portion, durable repository-specific
rules inside the project-context block, and subtree-specific rules in the
nearest nested `AGENTS.md`. README and background documentation are not loaded
automatically as instructions.

Use ownership maps or source-of-truth documents when ownership is ambiguous,
the change is cross-cutting, or a runtime-to-install path must be traced. For a
clear local owner, do not expand into unrelated ownership documents.

## Operating defaults

Use the smallest investigation, edit, communication, and validation that safely
completes the request.

- For clear implementation requests, start work without announcing a lane,
  plan, tool sequence, or validation intent.
- Do not narrate routine searches, edits, or successful checks. Report only a
  material scope expansion, conflicting task-relevant edits, a blocker, a
  safety or compatibility decision, or information the user requested.
- Do not ask for confirmation when the request is clear and safe.
- For reviews, rankings, brainstorms, recommendations, or “what would you fix”
  requests, return findings first and do not edit until the user asks.
- Ignore unrelated dirty-worktree changes, untracked files, generated outputs,
  and failures outside the accepted scope.
- Preserve unrelated local edits. If the target overlaps competing local work,
  compare the versions, keep the compatible task-relevant behavior, and avoid
  overwriting unrelated changes.
- Verify drift-prone facts only when the task depends on them. Examples include
  the current branch, remotes, installed paths, active processes, available
  recipes, and generated-artifact freshness.
- Do not mix cleanup, optional refactoring, dependency changes, formatting
  churn, release work, or generated-output changes into a focused fix unless
  one is required for correctness.
- Preserve established public, stored-data, configuration, security,
  installation, and compatibility behavior unless the user requests a change.
- Do not alter approval, permission, sandbox, patch-guard, stale-read,
  validation-gating, or execution-safety behavior as part of unrelated work.
- Read-only agents may investigate in parallel to help other busy agents. They
  may inspect relevant or adjacent contract surfaces but must not edit them;
  report findings to the busy agent, who retains edit ownership for the owned
  surface.

## Fast implementation path

Use this path for a bounded change with a clear owner:

1. Read the nearest applicable instructions.
2. Inspect the owning file and the smallest relevant surrounding code.
3. Trace at most one relevant caller, callee, registry, configuration owner, or
   installation hop when the connection is not already clear.
4. Inspect the nearest relevant test or existing reproduction.
5. Patch as soon as the defect or missing behavior is understood.
6. Review the focused diff.
7. Run the smallest check that proves the changed behavior.
8. Stop when the requested behavior is implemented and proven.

Do not scan the entire repository “just in case.” Do not enumerate every
entrypoint before editing a bounded owner. Do not repeatedly reread large files
after patching; inspect the focused diff and targeted context instead.

A focused implementation should normally use one inspection pass, one edit
pass, and one validation pass. When validation fails, diagnose that failure and
rerun only the invalidated check. Do not restart the investigation or broaden
the suite unless the failure reveals a wider owner or contract.

Expand beyond the fast path only when evidence shows that the change affects a
shared protocol, schema, generated artifact, lockfile, dependency, public API,
persistence format, security boundary, installation path, multiple runtime
registries, or unclear ownership.

If a new required owner appears after editing, inspect and update that owner.
Do not announce routine scope growth unless it materially changes risk, public
behavior, or the amount of requested work.

## Correctness and completion

For implementation work:

- Change the complete intended path, including a directly competing or replaced
  path that would otherwise continue winning at runtime.
- Do not leave task-relevant TODOs, placeholders, stubs, inert registrations, or
  known mismatches in the intended path.
- For bug fixes, prefer the original failing test, reproduction, or nearest
  owner test as proof.
- For broad claims such as “all,” “every,” “complete,” or “repo-wide,” perform a
  closure search appropriate to that claim. Do not perform a repo-wide closure
  sweep for a bounded request.
- When checking for bugs, do not stop at the first bug found, continue to collect all bugs then report/fix.

## Validation

Use the nearest sufficient proof and stop when it passes.

For lifecycle changes, run focused validation during implementation, then one appropriate broad validation and a normal disposable-host installation check after the final edit.

- Documentation or instruction wording: review the focused diff and use
  `git diff --check` only when whitespace or patch integrity is relevant.
- Behavior changes: run the closest owner test, focused test selection, or
  direct runtime reproduction.
- Tests do not automatically mean working code, do not assume tests equals success.
- Schema, protocol, package, lockfile, generated artifact, or installation
  changes: use the owning generator or official recipe.
- Do not stack build, test, lint, format, audit, install, smoke, and runtime
  checks unless each proves a distinct claim required by the task.
- Do not rerun an already-green source check unless a covered source or input
  changed.
- When possible, perform smoke-tests to make sure the new code actually works.
- Documentation, installation, generated inventory updates, and unrelated dirty
  paths do not invalidate a green source check unless they are declared inputs
  to that check.
- If unrelated dirty work blocks a focused proof, try supported scope or
  baseline isolation once. If that fails, report the limitation without
  broadening opt-outs or rerunning equivalent command variants.
- Tool success alone does not prove a runtime defect is fixed. Require the
  focused failing test or applicable user-visible/runtime evidence.

Final responses should state only:

- what materially changed;
- the validation that ran; and
- any known task-scope risk that remains.

Do not add a formal lane, completion-gate classification, or risk section when
there is no unresolved risk.
