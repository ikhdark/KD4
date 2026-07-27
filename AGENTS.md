# Repository policy

Shared policy revision: `2026-07-26.1`.

## Synchronization contract

The canonical shared-policy source is:

- `C:\Users\kuh\Desktop\kd4\AGENTS.md`

Its synchronized targets are:

- `C:\Users\kuh\Desktop\kds-main\AGENTS.md`
- `C:\Users\kuh\Desktop\mdpwa-main\AGENTS.md`
- `C:\Users\kuh\Desktop\kdsb-main\AGENTS.md`
- `C:\Users\kuh\Desktop\kdpc-main\AGENTS.md`
- `C:\Users\kuh\Desktop\kdgma-main\AGENTS.md`

Every byte outside the project-context block below must remain identical across
all six files. Only that block may contain repository-specific identity,
ownership, commands, validation, runtime, installation, safety, or protected
path details.

Change shared rules only in the canonical KD4 source. When a shared rule
changes, update its revision, copy the shared portion to all five targets in the
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
- Treat [`openai/codex`](https://github.com/openai/codex) as the official
  upstream. Merge upstream releases only for concrete improvements,
  compatibility, or local-fork repairs; do not merge solely because a change
  landed upstream.
- Identify the owning contract before editing protocol, app-server, SDK,
  configuration-schema, generated-artifact, or publish-path behavior. Update
  generated outputs only through the owning generator or recipe.

## Desktop app boundary

- The repository contains the Rust CLI and app-server components used by Codex
  Desktop, but not the native Windows desktop shell source.
- Source edits here do not hot-apply to the installed app. Desktop-visible
  completion requires rebuilding and updating or replacing the local binary,
  then restarting the Desktop app.

## Validation and local-build proof

- Rust crates: work from `codex-rs` and prefer the focused crate `just` recipe
  or focused Cargo check/test.
- App-server schema or protocol: run focused app-server tests and
  `just app-server-schema-check`. Use the force or raw generator recipes only
  for intentional contract regeneration.
- Configuration schema: run focused config/core tests and
  `just config-schema-check`. Use force or raw generator recipes only for
  intentional `codex-rs/core/config.schema.json` regeneration.
- Python SDK changes: use focused `uv run pytest` and `uv run ruff check .`;
  regenerate locks or artifacts only for touched SDK surfaces.
- Scripts: run syntax checks and the closest script tests; do not hand-edit
  generated locks such as `scripts/uv.lock`.
- Root maintenance: prefer matching root `package.json` scripts.
- Local publish: use `just publish-local-codex-dry-run` for path proof and
  `just publish-local-codex-final` before claiming installed replacement.
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
- Tooling success alone does not prove a runtime bug is fixed; require the
  focused failing test or approved final gate.

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
takes precedence inside its subtree. Never rely on an instruction file that is
absent from the working tree.

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
- Do not stop after the first fixes or rush to finish. Confirm test outcomes
  instead of assuming a test run is green, and do not treat green tests alone
  as completion. Continue until the complete task-relevant behavior is
  implemented correctly.
- When checking for bugs, do not stop at the first bug found, continue to collect all bugs then report/fix.
- Do NOT turn a directed fix into a broad fix. Stay focused on the task at hand.
