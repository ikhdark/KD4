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
- When edits overlap, preserve independent changes and combine compatible behavior
  against the requested contract. Verify the combined runtime path; ask only when
  conflicting intended behavior cannot be resolved from current evidence.
- Do not communicate with other agents from different sessions.
- Do not over-engineer implementations.
- Partial wiring of implemented code is forbidden, this is non-negotiable. End to end wiring is mandatory.
- After a full suite run, rerun only tests affected by a fix.
- Add no new frameworks, redesigns, cleanup projects, or extra acceptance checks unless a confirmed failure requires them.

### Validation
* Both of the following are mandatory*
1. Every test must assert an expected result and fail for a plausible incorrect implementation of the behavior or logic under test.
2. Repair weak tests covering the changed behavior or blocking its validation.
   Report unrelated weaknesses encountered without starting a broader test audit.

- If blocked by tests, do not repeat, simply finish the full task then report blocked by tests.

- Never run the full test suite unless specfically told to.

When validation or tests report errors, warnings, or failures, let the current run finish and diagnose all reported issues before making any repair edits. Then apply all related fixes in one consolidated batch and rerun each affected test or validation check once. Do not rerun checks that already passed and are unaffected by the repairs.


## Routing and task scope

- Before editing, identify the source owner and focused validation route. Reuse
  known paths and current evidence. For unresolved ownership or relationships,
  follow [source-map lookup instructions](SOURCEMAP.md#how-to-use-this-map);
  read the broad map only when a focused slice cannot resolve the boundary.
- Resolve material unknowns and relevant omitted relationships. Inspect exact
  evidence for affected callers/consumers, generated representations, and
  compatibility boundaries; record evidence paths or scoped no-matches.
- Use the owner's representative scenario and focused validation. Confirm the
  scenario enters through the normal boundary and asserts an independently
  expected observable effect, including absent effects for rejected or cancelled
  operations. Keep behavior-specific examples beside their owning tests.
- Modify the requested behavior and its affected contract relationships.
  `.codex/config.toml` owns local configuration.
- Use `just core-test-fast <target> <filter>` for local core tests and the relevant
  named `just core-gate <gate>` when its declared scope is needed. `core_lib`
  requires an explicit filter; use `--all` only with user authorization.
- After adding, deleting, moving, or renaming a repository file or directory,
  run `just source-map-check`, even when ownership prose is unchanged; it also
  rewrites the tracked-path snapshot.
