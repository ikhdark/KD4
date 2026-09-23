# O/F execution comparison and recovery repair

## Evidence and accounting

Read all 1,997 JSONL records in the four supplied files. Combine both sessions
per implementation; do not drop interrupted turns. The sessions selected and
implemented different KDA features against a concurrently changing checkout.
They are observational evidence, not matched performance trials.

| Metric | O combined | F combined |
| --- | ---: | ---: |
| Recorded usage events | 110 | 163 |
| Top-level tool calls | 106 | 156 |
| Input tokens | 14,267,034 | 12,681,019 |
| Cached input tokens | 13,918,720 | 11,713,664 |
| Uncached input tokens | 348,314 | 967,355 |
| Cached fraction of input | 97.56% | 92.37% |
| Output tokens, including reasoning | 68,127 | 88,661 |
| Sum of recorded turn durations | 2,613,791 ms | 4,143,690 ms |
| Compaction records | 0 | 0 |

Durations are summed work intervals, not elapsed time across overlapping sessions.
Usage totals come from the last cumulative token record in each file; duplicated
token-usage/event representations are not added together. O does not expose the
same detailed generation/tool timing schema as F. F records 162 logical
generations and 163 physical requests, including one stream retry. Its 372 tool
dispatches include orchestration and nested calls and cannot be compared directly
with O's 106 top-level calls.

F2's interrupted implementation turn contributes 1,160,492 ms, 45 logical
generations and 105 recorded tool dispatches. Its consumed tokens remain in the
cumulative totals. The interruption and subsequent user correction are neither
zero-cost nor successful completion. A `task_complete` event ends a turn; it does
not certify the overall repository task.

Derived metrics and compact, line-numbered traces are retained under the ignored
`_build/of-execution-audit/` directory. Record numbers below are one-based JSONL
lines. Encrypted reasoning and unrecorded provider requests cannot establish
causes on their own.

## Trace findings and disposition

| Observation | Causal assessment and action |
| --- | --- |
| F1 context drops from 82,121 to 37,545 input tokens at record 173; F2 drops from 80,005 to 34,935 at record 145. Cache misses accompany these and later edits. | History projection/freshness rewriting can invalidate a previously cached prefix. The active worktree already repairs that path with append-only invalidation notices. Preserved that work; no second cache mechanism or new cache thresholds. |
| F has 16 explicit artifact-recovery calls. F2 records 457, 463, 469, 475 and 481 inspect one 545,932-byte diagnostic artifact through different selectors. F1 also recovers omitted source and failure details. | Recovery is useful when required evidence was omitted. Treating every recovery generation as redundant would suppress necessary diagnosis. Current source already has typed overflow, exact reconstruction, and bounded recovery. A remaining repeated-I/O defect inside that recovery path is repaired below. |
| Nested command output includes process receipts and recovery metadata; large batched results can truncate twice. | A process receipt is required when script completion differs from process completion or when the script prints only selected output. Removing it indiscriminately would break resumed-handle invalidation and history handling. Existing command projection/failure work is concurrently being changed; it is excluded from this repair. |
| F1 records 399–419 inspect a failed test launch and wait for another process; F2 records 507–536 inspect overlapping validation and later run focused tests. | Concurrent processes, changing source, and the user's prohibition on the full suite affect required waits and validation. These intervals are not proof of a scheduler regression. No legitimate wait, retry, test, or generation is suppressed. |
| Repeated source reads follow mismatched patches and changes by another session. | Some repeats obtain current evidence, and the active worktree already contains patch-retry/workspace-transaction changes. No blanket read memoization or patch special case. |
| F1 records a disconnected response and retry around 431. | A transport retry is correctness work, not an unnecessary generation merely because it increases request count. Accepted-output retry state and generation scheduling remain unchanged. |
| No compaction, no no-progress directives, and no goal-driven continuation appear in these four traces. | Compaction loss, convergence telemetry, goal loops, and false goal completion cannot explain the observed differences here. Existing task-retention and interruption fixes are not reimplemented. |

The larger F request count and duration do not isolate implementation overhead:
different features, compile failures, concurrent mutations, and extra user turns
also require work. The fork's lower aggregate input count likewise does not prove
greater efficiency when its uncached input and recovery work increase.

## Implemented root-cause fix: one snapshot per recovery transaction

Production path:

`ReadToolOutputHandler` → `handle_read_tool_output` →
`execute_recovery_transaction_with_continuations` → selector admission →
`RecoveryContinuationState` → next required page → exact reconstruction and
bounded result delivery through direct tools or code mode.

Previously both initial selection and every uncached continuation invoked
`read_tool_output_selectors_with_ceiling_and_reuse`. Despite its name, that
function always returned `reused = false`: it reopened metadata, opened artifact
segments, read **all retained bytes**, verified SHA and line indexes, and only
then selected the requested range. The transaction discarded the validated
snapshot, so selecting the next few kilobytes repeated whole-artifact I/O,
hashing, allocation, and blocking-task dispatch. Existing page reuse only helped
overlapping ranges that had already been delivered; it could not reuse the
undelivered bytes already read for validation.

The repair retains the existing validated metadata and immutable bytes in an
`Arc<ToolOutputSnapshot>` for one recovery invocation. Initial selection and all
internally followed pages use the same selector implementation against that
snapshot. Metadata loading and validation now share one blocking task; selection
stays off async runtime workers. No cross-call cache, new limits, benchmark
recognition, or new scheduling machinery is introduced.

For a recovery with N newly followed pages, whole-artifact loads change from
1 + N to 1. This improves actual local execution; it does not claim to remove a
model generation or quantify an end-to-end latency gain from these logs. The
traces establish that artifact recovery is exercised, while source inspection
and deterministic reproduction establish this internal cost.

Correctness retained: identifier/thread lookup, writer-lock checks, SHA and line
index validation at the beginning of every invocation; exact selector bounds;
output ceilings and resumable overflow; cancellation between pages; selection
identity checks; canonical output; and required later recovery calls. Once
validated, one invocation observes immutable bytes consistently even if its
backing artifact expires while it is selecting pages. A later invocation must
read and authenticate storage again. The initial artifact read remains accounted
for; the transaction's reuse flag remains false rather than hiding that work.

Regression checks:

- `recovery_pages_use_one_validated_snapshot_without_reopening_the_artifact`
  removes the backing file after loading, then runs the production drain loop.
  It must return exact authenticated pages, preserve identity, stop at the normal
  budget, honor cancellation without following pages, and reject a new invocation
  after expiration. This fails if continuation selection reopens storage.
- `new_recovery_transaction_revalidates_same_length_modified_bytes` first
  returns complete exact output, then replaces the file with equal-length data.
  A fresh production invocation must reject the SHA mismatch.

## Validation

Both new behavioral regression tests passed through the repository's Rust test
runner (Nextest run `0ba1fa6a-97b1-4c71-873e-54ccb69db8b8`). The deletion test
executes the production continuation loop and compares delivered byte ranges;
the corruption test executes separate production recovery transactions.

The runner's existing helper-selection table now identifies the artifact and
recovery unit-test modules as needing no external helper binaries. These tests
use local artifacts and the in-process session fixture. Unknown modules retain
the conservative helper set.

Earlier attempts did consume compilation and queue time: a stale workspace
lockfile, concurrently incomplete workspace-tool dependencies, and a transient
shell-spec syntax error prevented test execution. Those prerequisite problems
were resolved before the successful run. Another queued attempt was terminated
before running tests; it is not a passing or failing test result. Logs are under
`_build/of-execution-audit/`. The successful build reported unrelated dead-code
warnings in context-projection code, with no recovery-code warnings.

The broader runner invocation successfully built its test inventory, then its
execution rebuild encountered two concurrently stale `SamplingProjectionAnchor`
fixtures. Both fixtures were already corrected when inspected. A subsequent
retry was terminated during compilation, before executing tests. Neither run is
counted as test success.

To finish validation without another changing-worktree rebuild, ran the existing
suites from the successfully built `codex_core-a27b9c8d51f699e4.exe` test binary.
That binary was built after both recovery source files were last changed. The
Windows test environment matched the runner's setup: cleared inherited `CODEX_*`
values, an 8 MiB Rust test-thread stack, and the core crate working directory.
Tests ran serially, without rerunning the two already-passing regressions:

- Artifact unit tests and recovery handler tests: **50 passed**, zero failed or
  ignored (`recovery-suites-built-binary.log`).
- Artifact hardening tests: **78 passed**, zero failed or ignored
  (`artifact-hardening-built-binary.log`). These include stale SHA/size
  rejection, expiration, exact multi-selector recovery, retention, streaming
  ownership, cancellation, and bounded subdivision.
- Pinned rustfmt check and whitespace checks passed for the changed recovery
  files; the test-runner configuration also passed the whitespace check.

Thus 130 distinct tests passed. The deterministic production-loop simulation
demonstrates the failure-pattern improvement: continuation pages succeed from
authenticated bytes after storage is removed, while a new invocation still
fails. This is not a claim that the latest entire concurrently changing worktree
was rebuilt successfully, nor an end-to-end model-latency benchmark.

No binary replacement, Desktop restart, publication, or upstream synchronization
is part of this work.
