## Repository identity and runtime boundary

- This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Upstream synchronization or distribution requires a request that explicitly names it.
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


#### Scope and workspace

- Ask questions in plain language when clarity is needed, do not continue to ask questions after implementation has begun.
- When edits overlap, compare the versions and pick the most capable one and move on.
- Do not communicate with other agents from different sessions.
- Do not over-engineer implementations.
- Partial wiring of implemented code is forbidden, this is non-negotiable. End to end wiring is mandatory.
- After a full suite run, rerun only tests affected by a fix.
- Add no new frameworks, redesigns, cleanup projects, or extra acceptance checks unless a confirmed failure requires them.

### Validation
*All three of the following are mandatory*
1. Do not create a test that does not prove direct behavior and/or logic. 
2. Every test must prove direct behavior and/or logic. 
3. If you find a test that does not follow this policy, fix it.

If you need a more detailed verison:
- For behavior changes, identify the normal entry point, input, expected observable result, and a plausible incorrect implementation the test rejects. Strengthen the nearest sufficient scenario with independent contract expectations and consumer-visible effects, including forbidden side effects on failure. Use normal registration for wiring claims; doubles may replace external dependencies, not the behavior under test. Run the existing narrow target/filter and report the behavior proved; unavailable prerequisites are unverified. For documentation-only changes, run the nearest relevant existing validation instead of creating a test.

- If blocked by tests, do not repeat, simply finish the full task then report blocked by tests.

- Never run the full test suite unless specfically told to.

- When validating do not fix errors one by one, wait until the test completes, then you are allowed to fix them in batches.


## Routing and task scope

- Before reading `SOURCEMAP.md` broadly, query the smallest named owner slice:
  `python scripts/source_owners.py slice --owner <owner-id> --focus "<task
description>" --max-relationships 32`. Require an untruncated result with no
  omitted relationships or material unknowns, then read its exact evidence
  locations. Use its representative scenario and focused validation to replace
  broad test searches; confirm the scenario enters through the normal boundary
  and asserts an effect. Examples: the inventory benchmark independently expects
  `TOTAL: 5`; its duration verifier distinguishes `1s` from `1 s` and ASCII from
  non-ASCII digits. Plan rejection checks absent stored state and cancellation
  checks absent updates; model-override scenarios inspect the actual config file
  after shutdown. Reuse these proof patterns, not their incidental setup.
  Read the broad map only when no owner matches or the slice leaves
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
