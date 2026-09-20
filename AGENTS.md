# Repository instructions

## Repository identity and runtime boundary

* This is the user's local fork of [`openai/codex`](https://github.com/openai/codex).
  Upstream synchronization or distribution requires a request that explicitly names it.
* This is a local project for the user's own use and is not intended for public release or distribution. Its main goal is to improve and optimize Codex.
* Treat the active repository root as the checkout location; do not hard-code a workstation-specific checkout path.
* `C:\Users\kuh\Desktop\LOCAL-KD` is the fork home and `C:\Users\kuh\.codex` is the official upstream home. The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`.
* This repository contains the Rust CLI and app-server, not the native Windows shell. Source changes become Desktop-visible only after rebuilding and replacing or updating the local binary, then restarting Desktop. Perform those activation steps only when the request includes them.

## Scope and workspace

* A no-change result is valid and preferred when the requested capability already exists adequately. Before adding a mechanism, identify the concrete missing capability and explain, using relevant source evidence, why existing abstractions are insufficient. Prefer reuse, consolidation, or deletion over adding parallel machinery.
* When edits overlap, preserve independent changes and combine compatible behavior against the requested contract. Verify the combined runtime path; ask only when conflicting intended behavior cannot be resolved from current evidence.
* Partial wiring of implemented code is forbidden. End-to-end wiring is mandatory.
* Add no new frameworks, redesigns, or cleanup projects unless a confirmed failure requires them. Add acceptance checks only when required to validate the requested behavior or a confirmed failure.

## Validation

* Every test must assert an expected result and fail for a plausible incorrect implementation of the behavior or logic under test.
* Repair weak tests covering the changed behavior or blocking its validation. Report unrelated weaknesses encountered without starting a broader test audit.
* When validation or tests report errors, warnings, or failures, let the current run finish and diagnose all reported issues before making repair edits. Apply related fixes in consolidated batches and rerun affected tests or validation checks after each batch. Repeat only if failures remain or new evidence requires it. Do not rerun checks that already passed and are unaffected by the repairs.

## Routing and task scope

* Route by symptom before routing by ownership. A failure reported in the running Desktop or CLI is a runtime symptom: read that installation's own logs, local state, and binary version first, because the installed binary can lag this checkout. Use the source map once a repository change is actually in scope.
* When changing repository source, use the [source-map lookup instructions](SOURCEMAP.md#how-to-use-this-map) to identify owners, relevant source, and validation routes. Enter by path (`slice --path <file>`), which resolves owners without guessing an id. Pass `--owner <owner-id>` only with an id returned by a slice or by `source_owners.py list`; owner ids are not inferable from crate or directory names (`app-server` is invalid, `app-server-runtime` is the owner).
* Inspect additional callers, consumers, generated representations, or compatibility boundaries only when they could materially affect the requested change. Reuse still-current evidence instead of repeating discovery.
* Use focused validation that reaches the changed behavior through its normal runtime boundary and checks the expected observable result. Include rejected, cancelled, or absent effects when those are part of the changed behavior.
* `.codex/config.toml` owns local configuration.
* For source inventories, use `python scripts/source_inventory.py --query <query.json> --state <task-state.json> --report <report.md>` with task-owned files outside the source tree. Declare `required_categories` up front to retain coverage questions. Categories declare `name`, `paths` globs, and optional `contains`. For ordinary file/rule inventories, explicitly select `verification: "path"`; use content rules and inspect ambiguous classifications as needed. Select `verification: "runtime"` when the requested conclusion depends on runtime use; it requires per-file include/exclude decisions with source hashes, review reasons, and exact hashed consumer-line evidence. The helper retains its conservative runtime fallback when verification is omitted, so always choose the mode that matches the requested claim. A path match does not establish runtime use. `unresolved: false` does not verify a runtime classification. Reuse the state, inspect only unresolved records or missing categories, then link the generated report instead of retyping filenames. Use `--state <task-state.json> --render-only --report <report.md>` to render retained results without rescanning; rerun discovery only when relevant inputs or scope change. Counts and paths come from the same records. Run bounded read-only inventories directly without a plan.
* Use `just core-test-list` to discover named test targets and gates, then `just core-test-fast <target> <filter>` for local core tests or `just core-gate <gate> [<gate> ...]` when those declared scopes are needed. `core_lib` requires an explicit filter; use `--all` only with user authorization. `just help` lists other recipes. Recipes default to `codex-rs` as their working directory unless overridden; Python is required by the recipe shell. Build/test parallelism defaults to 8 via `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, and `NEXTEST_TEST_THREADS`, with existing environment overrides preserved.
* For exact inference-request diagnostics, set `CODEX_ROLLOUT_TRACE_ROOT` on the process being measured; see the rollout-trace discussion in `SOURCEMAP.md`. `prompt-debug` is an offline reconstruction, not proof of a production request.
* Inventory summaries return immutable report and canonical path artifacts. Use `--render-only --remaining` to page unresolved evidence; use category `json_summary: true` for JSON fields, types, and lengths without printing prompt bodies. Scope revisions require `scope_change: {from_query_id, reason}` and retain earlier unresolved/excluded scope in the report. Do not imply earlier coverage is complete from a narrower query.
* For instruction discovery below the current directory, use `python scripts/source_inventory.py --instructions <repository-relative-scope> ...` or equivalent bounded ancestor checks and scoped enumeration. The helper checks ancestor instructions and prunes build/dependency directories before descending. Read the applicable returned files before editing.
* After adding, deleting, moving, or renaming a repository file or directory, run `just source-map-check`, even when ownership prose is unchanged; it also rewrites the tracked-path snapshot.
