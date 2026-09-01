# KD4 repository instructions

## Repository identity and runtime boundary

- This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Operate only on fork-local source and artifacts. Upstream synchronization or
  distribution requires a request that explicitly names it.
- This is a local project for the user's own use and is not intended for public
  release or distribution. It's main goal is to improve and optimize codex.
- Treat the active repository root as the checkout location; do not hard-code a
  workstation-specific checkout path.
- `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and
  `C:\Users\kuh\.codex` is the official upstream home. The published fork
  Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
- This repository contains the Rust CLI and app-server, not the native Windows
  shell. Source changes become Desktop-visible only after rebuilding and
  replacing or updating the local binary, then restarting Desktop. Perform
  those activation steps only when the request includes them.


### Scope and workspace
- Created tests must prove the direct behavior change and end to end wiring.
- Ask questions for clarity before implementing.
- Read the root `AGENTS.md` in full, and read every user-provided or user-named
  file in full.
- Communicate with the user in very plain language.
- Do not publish, deploy, or modify upstream state unless the user explicitly
  requests that action.
- When you encounter overlapping edits, choose the best version. Implement your
  version when it is better, keep the current or concurrently changing version
  when it is better, and combine them when that produces the best result. If
  the existing version is already better than your proposed edit, leave it
  unchanged and move on.

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
- `SOURCEMAP.md` covers workspace and maintenance-script routing;
  `.codex/config.toml` and `.codex/skills` own local configuration, fork-local
  skills, and validation workflows.
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