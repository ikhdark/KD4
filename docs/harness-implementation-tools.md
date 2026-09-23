# Implementation tools

These changes are local harness capabilities. They do not establish a measured
F-versus-O speedup; compare identical tasks and source snapshots to measure that.
Rebuilding and installing the fork binary, then restarting Desktop, is required
before source changes affect the published Desktop.

The semantic/validation worker and automatic workspace snapshots have been
removed; see [retired task workspaces](workspace-transactions.md).

| Change | Implemented behavior |
| --- | --- |
| 1. Whole-patch preflight | Finds rejected update hunks before committing a valid prefix. |
| 2. Revision-bound edits | Replaces exact source ranges without regenerating surrounding code; stale hashes reject. |
| 3. Phase checkpoints | Replaces selected completed outputs with recovery receipts while preserving active evidence. |

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

## Whole-patch preflight and revision-bound edits

All update hunks and deletes are prepared before the first write. An initial source
conflict returns all detected conflicts without committing an earlier valid
hunk. Files are checked again when applying prepared edits. Unexpected I/O
failures, concurrent external edits during application, or cancellation can still
leave an explicitly reported committed delta.

`read_file` returns the complete file's `source_sha256`. Replace an inclusive
original line range without regenerating surrounding source:

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

## Evidence-preserving phase checkpoints

`context_checkpoint` takes a factual `summary`, `active_work`,
`completed_call_ids`, and disjoint `retained_evidence` IDs. Only consumed,
successful, complete artifact-backed results qualify. Unknown, unread or failed
results are rejected. Selected outputs become exact recovery receipts in the
next model request; canonical history is retained. Unselected evidence and the
original instructions remain available. The checkpoint persists with the session
and deliberately creates one new prompt projection boundary.

These retained mechanisms address patch retries, repeated source reconstruction,
and accumulated completed-phase context. Mechanism tests do not establish a
measured wall-time, token, or task-success improvement.

## Historical validation recorded on 2026-09-22

The following records the implementation before retirement of the worker and
snapshots; it is not validation of the current source.

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
