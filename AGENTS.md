# Repository policy

Policy revision: `2026-07-29.1`.

## Project context

<!-- PROJECT-CONTEXT: START -->
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
  keeping changes reviewable, local-build focused.

### Source of truth and ownership

- Known top-level instruction scopes include `.codex/AGENTS.md`,
  `codex-rs/AGENTS.md`, and `scripts/AGENTS.md`; further nested files apply
  only where present.
- `.codex/AGENTS.md` and `.codex/README.md` own workspace policy and routing.
  `.codex/config.toml` owns optional repo-local runtime configuration,
  `.codex/environments` owns worktree setup/state, and `.codex/skills` owns
  fork-local skills and validation workflows.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns the checked repository inventory, runtime
  entrypoints, package and Rust-domain routing, build/package/publish paths,
  generated contracts, validation routes, and cross-cutting change routes.
- Treat the maintenance contract in [`SOURCEMAP.md`](SOURCEMAP.md) as required:
  update the map in the same change whenever repository structure, ownership,
  entrypoints, contracts, generated outputs, or build/install routes materially
  change, then run `just source-map-check`.

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

## Instruction scope

Budget is not a constraint. Do not reduce investigation quality, skip needed code reading, or stop early to conserve tokens, context, commands, or compute.

Read the closest relevant `AGENTS.md`. A nearer file augments this policy and
takes precedence inside its subtree. Never rely on an instruction file that is
absent from the working tree.

When the user presents a file path, read the entire file.

Use ownership maps or source-of-truth documents for a
clear local owner, do not expand into unrelated ownership documents.

You are encouraged but not required to use the MCP/Plug-in "Repo-Atlas" when you need to understand the workspace.

## Operating defaults

Use the fastest investigation, edit, communication, and validation/tests that safely
completes the request, keep correctness with speed.

- When working on a task, ignore ALL UNRELATED FILES.
- Do NOT start implementing until you COMPLETELY UNDERSTAND the required files/folders/code/workspace that you will need to know for the task, budget is not a concern, do not skip out on exploring and understanding to simply make things cheaper.
- Update files that mentioned the file you deleted.
- You are allowed to improve the implementation while doing so if you have gathered enough proven information on the task.
- Overlapping edits from other agents are expected. Compare competing versions once, keep or combine the best compatible task-relevant behavior, coordinate directly when needed, and continue with the rest of your assigned task.
- Read-only agents are encouraged but not required for any task invovling a high amount of files.
- Do not loop on unchanged checks or repeatedly revisit completed
  work.
- When checking for bugs, do not stop merely because the first bug was found. Continue
  across the accepted scope until relevant surfaces have at least been surveyed and the
  remaining candidates are confirmed, rejected, duplicated, deferred with a concrete
  missing fact, blocked, out of scope, or disproportionately expensive to resolve
  relative to their likely value. Never invent or pad findings to satisfy an expected
  count.
- Do NOT turn a directed fix into a broad fix.
- You are required to do one "double check" at the end of your task, a "double check" is simply reviewing your own code and making sure no bugs have been overlooked or created, fix whatever you find in your double check.
- If while implementing you run into validation/test/implementation blockers, fix them but make sure to remember there will most likely be multiple agents working, so do not fight over blockers to simply complete your task, stop and think collectively on the best answer to the problem. 