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



A no-change result is valid and preferred when the requested capability already exists adequately. Before adding a mechanism, identify the concrete missing capability and prove that existing abstractions cannot satisfy it. Prefer reuse, consolidation, or deletion over adding parallel machinery.

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

- Do not repeat unchanged failing tests. After a relevant implementation, test,
  or environment repair, rerun the affected checks once. If they remain blocked,
  finish independent work and report the blocker.

- Never run the full test suite unless specfically told to.

When validation or tests report errors, warnings, or failures, let the current run finish and diagnose all reported issues before making any repair edits. Then apply all related fixes in one consolidated batch and rerun each affected test or validation check once. Do not rerun checks that already passed and are unaffected by the repairs.


## Routing and task scope

- Before reading, grepping, or globbing to locate code for a coding task, call
  Repo Atlas `task` with the task text, then read its returned files and owner
  instructions. Known files are anchors, not a reason to skip the call. Reuse
  that result for the same task and source snapshot. Select the repository being
  worked on with `select_root` when it differs from the current Atlas root.
  Use the [source-map lookup instructions](SOURCEMAP.md#how-to-use-this-map)
  for ownership details the result leaves unresolved, or if Atlas is unavailable.
- For Rust changes in `codex-rs`, use KDA: after editing Rust, call `test_plan`
  with `base: "HEAD"` and run its selected tests through KDA `test`. Before
  reporting a Rust change complete, call `review` with `base: "HEAD"` on the
  changed items and resolve its obligations. Run the KDA gate (`gate_start`,
  then `gate_poll`) when the task requests a gate or the change touches unsafe,
  atomic, FFI, or allocation code. KDA evidence is bounded; read its coverage
  and verdicts, and it does not replace the required validation commands.
- Resolve material unknowns and relevant omitted relationships. Inspect exact
  evidence for callers/consumers, generated representations, and compatibility
  boundaries when the change could affect them. Reuse still-current evidence
  already collected; record paths or scoped no-matches for material unknowns.
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
