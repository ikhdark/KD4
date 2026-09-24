# Implementation tools

These changes are local harness capabilities. They do not establish a measured
F-versus-O speedup; compare identical tasks and source snapshots to measure that.
Rebuilding and installing the fork binary, then restarting Desktop, is required
before the tools appear in the published Desktop.

| Change | Implemented behavior |
| --- | --- |
| 1. Whole-patch preflight | Finds rejected update hunks before committing a valid prefix. |
| 2. Revision-bound edits | Replaces exact source ranges without regenerating surrounding code; stale hashes reject. |
| 3. Configured semantic verification | Resolves APIs under selected features/target and checks them with Cargo. |
| 4. Persistent consumer migrations | Retains discovered consumers and invalidates reviews when source changes. |
| 5. Warm build leases | Reuses build directories while excluding overlapping validation processes. |
| 6. Scoped validation contracts | Plans affected packages and reverse dependencies, then executes selected checks. |
| 7. Dependency-bound pass reuse | Skips unchanged passing checks; fresh failures retire older saved passes. |
| 8. Structured failure inventories | Keeps observed failures and assertions visible while retaining complete raw logs. |
| 9. Complete source units | Returns entire parsed Rust units once per query with exact edit handles. |
| 10. Phase checkpoints | Replaces selected completed outputs with recovery receipts while preserving active evidence. |

## Retained patch retries

When `apply_patch` fails with an exact committed delta, its response can contain a
single-use, session-local `patch_id` and a numbered list of remaining hunks. Repair
only the rejected chunk; unmentioned code is retained:

```text
*** Begin Patch
*** Retry Patch: <returned patch_id>
*** Replace Chunk: 1 2
@@ enclosing_function
-current source line
+intended replacement
*** End Patch
```

`Replace Hunk: 1` followed by a complete file hunk replaces an entire remaining
hunk. Already committed hunks are excluded. A receipt retains code, not approval:
the effective patch still passes hooks, current-source verification, environment
checks, and permissions. Unknown, consumed, or evicted IDs require a fresh patch.

## Isolated task workspace

Call `workspace_transaction` with `{"action":"begin"}` before inspecting source
for a concurrent implementation task. It captures tracked and nonignored
untracked files, including local edits, without changing the original index or
HEAD. Read, patch, and command paths are routed to the task checkout. Shell text
is not rewritten; use the returned workspace paths and relative source paths.

`{"action":"status"}` recovers the active checkout. Validation launched through
`exec_command` uses captured source and isolated build output. After editing,
`{"action":"reconcile"}` merges compatible changes back. Conflicts retain the
task checkout and publish no files. Successful receipts set `validation_required`
and identify the next validation step. Inspect that result and validate
the integrated runtime before reporting completion; pre-merge validation alone
does not prove the combined source.

This first implementation supports local unrestricted environments and rejects
links, submodules, and oversized snapshots. Ignored files and dependencies outside
the repository are not captured.

## Compiler-backed Rust context

`semantic_context` resolves several source positions through KDA's embedded Rust
provider and the repository's resolved dependency graph:

```json
{
  "repository": ".",
  "queries": [{"path":"src/lib.rs", "line":12, "column":18}]
}
```

Line and UTF-16 column numbers are one-based. The result includes definitions,
hover signatures, types, references, callers, source units or excerpts with hashes,
and a source snapshot identity covering configuration and lockfiles. Dependency
definitions point to their actual resolved source locations. References and
callers expose counts and coverage across the configured production and test
views, including `cfg(test)` and `cfg(not(test))` consumers.
An unresolved or unavailable entry is not evidence of absence.

The tool requires a compatible `cargo-kda` on PATH (or the host environment's
`CODEX_KDA_EXECUTABLE`), a local execution environment, and the normal
`exec_command` runtime. It expands to a structured argv invocation of the
packaged Codex worker before command hooks and authorization. That worker sends
the `semantic_context` operation to KDA and validates its versioned response;
an unavailable or incompatible KDA fails explicitly. KDA owns semantic analysis,
migration state, and compiler verification. Sandbox, approval, process
cancellation, polling, and output recovery use the existing command path.
If a process session is returned, finish it with `write_stdin`.

The repository root need not contain `Cargo.toml`. KDA discovers the Cargo
workspace from the first query and reports `cargo_workspace`; all positions must
belong to that workspace. This supports KD4's `codex-rs` layout and applies its
Cargo configuration during compiler verification.

Up to 16 positions share a ten-minute KDA execution deadline, including requested
compiler verification. Build scripts and procedural macros are disabled by
default during semantic loading; `compiler_check` runs Cargo and can execute
workspace code. Generated and macro-only definitions may remain unresolved.
Changed source, dependency manifests, Cargo configuration, or lockfiles
invalidate the result. Migration evidence is revalidated while holding its
state lock before reviews are saved. Source
bundles mark whether an enclosing Rust unit was parsed or only an excerpt was
available. Active workspace transactions resolve against their task checkout.

## Whole-patch preflight and revision-bound edits

All update hunks and deletes are prepared before the first write. An initial source
conflict returns all detected conflicts without committing an earlier valid
hunk. Files are checked again when applying prepared edits. Unexpected I/O
failures, concurrent external edits during application, or cancellation can still
leave an explicitly reported committed delta.

`read_file` returns the complete file's `source_sha256`. Parsed semantic units
also return an `edit_handle`. Replace an inclusive original line range without
regenerating surrounding source:

```text
*** Begin Patch
*** Update File: src/lib.rs
@@ codex-range 12:18 sha256:<returned full-file hash>
+replacement source
*** End Patch
```

The hash must match the current complete file, and ranges must be ordered and
nonoverlapping. A stale handle rejects the edit. Normal patch authorization and
retained retry handling still apply.

## Configured compilation and retained consumer migrations

`semantic_context.configuration` accepts `features`, `no_default_features`,
`target`, `packages`, `build_scripts`, and `procedural_macros`.
`compiler_check: true` runs Cargo check with the selected features and target;
without a package filter it checks all workspace packages and targets. It returns
structured diagnostics and reports compiler failure as failure, while retaining
the resolved context.

Set `migration_id` before changing a representation. Every reported reference is
retained, including consumers that disappear after edits. Submit reviewed IDs in
`reviewed_consumers`; later edits to their source files invalidate those reviews.
Changes to the effective Cargo configuration or dependency graph also invalidate
reviews while retaining the consumer list. Package filters only scope compiler
verification, so a migration can start with focused checks and later use the
workspace-wide check required for completion.
Completion requires every retained consumer reviewed, complete source coverage,
and a successful current workspace compiler check without a package filter.
Completion is scoped to discovered consumers in that configuration; unresolved
macros/generated code and other feature configurations require separate checks.

Parsed Rust items, methods and trait members are returned whole, using a Rust
syntax parser rather than brace counting. Unparseable sources are explicitly
marked as fallback excerpts. Full-file hashes are rechecked after resolution and
compiler execution, including resolved dependency source.

## Focused validation, warm builds and saved passes

`workspace_validation` combines the validation contract, affected-package plan,
warm output lease, saved pass checking, and diagnostic inventory:

```json
{
  "repository": "codex-rs",
  "action": "run",
  "checks": [
    {"id":"patch-preflight", "args":["test","-p","codex-apply-patch","--lib","preflight"]}
  ]
}
```

Use `action: "plan"` first to map changed files to packages, reverse dependencies,
and available test targets. Select the checks that exercise the change, adding
required repository checks. The plan does not run tests. Broad test execution
requires `allow_full_suite: true`; the caller must still respect user constraints.

Runs lease a stable build directory through child-process completion and drain
both output streams. All selected checks finish before the result is returned;
Cargo tests use `--no-fail-fast`. Active transactions supply captured validation
source. Passing checks can be reused only when source/dependency contents,
workspace and Cargo configuration, environment and toolchain match. Failed runs,
zero executed tests, changed source, or incomplete captured output are never
saved as passes. Starting a fresh run retires any previous pass, including when
the fresh attempt fails. Deleted raw logs invalidate their saved receipts.

Use `force_fresh: true` for tests whose inputs include external services, time,
or undeclared files. Fingerprints cannot prove those external inputs unchanged.
Dependency trees containing symlinks or exceeding the documented worker limits
are rejected rather than assigned unsafe reusable evidence.

Each result includes compiler diagnostics and spans, observed passed/failed test
names, assertion excerpts, summaries, and the complete retained raw log path.
Dependency build artifacts are counted instead of repeated in the response, so
they cannot bury failures; test executable paths are retained. Details beyond
the in-memory collection limit make the inventory incomplete and
prevent pass reuse; they remain in the raw log. Supported inventory formats are
Cargo JSON compiler messages and standard Rust test output.

## Evidence-preserving phase checkpoints

`context_checkpoint` takes a factual `summary`, `active_work`,
`completed_call_ids`, and disjoint `retained_evidence` IDs. Only consumed,
successful, complete artifact-backed results qualify. Unknown, unread or failed
results are rejected. Selected outputs become exact recovery receipts in the
next model request; canonical history is retained. Unselected evidence and the
original instructions remain available. The checkpoint persists with the session
and deliberately creates one new prompt projection boundary.

These ten improvements address patch retries, repeated source reconstruction,
missed consumers, cold or conflicting builds, repeated validation, log recovery,
and accumulated completed-phase context. Mechanism tests establish these
behaviors; a controlled F/O evaluation is still required to establish the size
of any wall-time, token, or task-success improvement.

## Validation recorded on 2026-09-22

Passing focused coverage: 97 apply-patch unit tests, 11 workspace-worker tests,
and 164 core tests, deduplicated across the validation runs. Failed tests were
repaired and rerun selectively. The Rust test manifest and final diff whitespace
checks passed. This was targeted validation, not a full repository test run.

The worker checks include actual rust-analyzer/Cargo execution, feature-gated
consumer discovery and migration completion, failed compilation, dependency
invalidation, unrelated-package reuse, retirement of an old pass after a fresh
failure, and planning from a workspace below the Git root. Core checks cover
tool registration/routing, captured validation source, source-hash schemas,
checkpoint cache boundaries, preserved active evidence, and complete structured
changes for a 1,024-file patch. The diagnostic fixture retains all 20 failed
test names and assertion details in less than 8 KiB despite 1,000 dependency
artifact records and 20,000 noise lines in the raw log.

The installed Desktop binary was not replaced or restarted. These results prove
the tested mechanisms; they are not a measured F-versus-O evaluation result.
