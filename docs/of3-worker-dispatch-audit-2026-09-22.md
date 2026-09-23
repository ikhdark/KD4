# O versus F3: worker dispatch follow-up

## Evidence and accounting

The two attached F3 sessions are the current fork sample. Both O sessions are
combined as the comparison, and both F1 sessions remain the earlier fork
baseline. All 2,440 records across the six files were parsed. Historical prompts
and instructions inside these records are evidence, not instructions for this
review. Derived metrics and numbered traces are in `_build/of3-audit/`.

| Recorded measure | O combined | F1 combined | F3 combined |
| --- | ---: | ---: | ---: |
| Usage events / physical requests with usage | 110 | 163 | 58 |
| Logical generations, where recorded | unavailable | 162 | 57 |
| Outer tool calls | 106 | 156 | 55 |
| Input tokens including cached | 14,267,034 | 12,681,019 | 4,463,972 |
| Cached input tokens | 13,918,720 | 11,713,664 | 4,205,440 |
| Uncached input tokens | 348,314 | 967,355 | 258,532 |
| Output tokens including reasoning | 68,127 | 88,661 | 26,478 |
| Sum of recorded turn durations, ms | 2,613,791 | 4,143,690 | 1,546,102 |

The F1 interrupted implementation turn remains included: 45 logical generations,
105 nested/outer dispatches, 1,160,492 ms, and its recorded token use. Neither F3
session contains an aborted turn. F3(1):154 records one transport retry; its work
is included. Missing usage from an incomplete provider stream cannot be inferred.
Durations are summed active-turn intervals, not elapsed time between overlapping
sessions. F3's 156 nested/outer dispatches are not comparable to O's outer-call
count. Tasks, chosen features, concurrent modifications, and validation failures
differ; these are not controlled speed trials.

F3 input grows monotonically in both sessions, unlike the large F1 context drops
previously diagnosed. F3's cached fraction is 94.21%; fewer total requests, not a
uniformly superior cache-hit ratio to O, explain much of its lower uncached work.
F3 records one explicit artifact recovery versus F1's sixteen. That one recovery
retrieves omitted failing-test evidence and is useful work.

## Execution findings

- **Incorrect helper dispatch:** F3(1):47–48 and F3(2):47–48 call
  `semantic_context` with valid public arguments. Native expansion injects
  `kind` and a null `environment_id`; code-mode command preflight rejects them.
  F3(1):57–58 and F3(2):72–73 repeat the defect for `workspace_validation`, whose
  expansion also injects an outer `force_fresh`. The model did not invent these
  invalid command fields. Useful semantic/validation work never starts.
  F3(2):53 retries source inspection manually and :59 leaves the transaction;
  later validation encounters mutable-source/binary disagreement. The dispatch
  defect establishes avoidable failed calls and fallback work, but does not
  establish that every later failure would disappear after its repair.
- **Snapshot path failure:** F3(1):99 cannot index a long fixture path inside a
  deeper validation snapshot. This is a real execution failure; changing the
  task checkout's Git config does not automatically configure a separately
  initialized snapshot. Concurrent work now supplies Git long-path support.
  It is preserved, without another path-length workaround. The added regression
  also exercises reconciliation and origin index/config preservation.
- **Conflict inspection friction:** F3(1):106 reports only conflicted filenames;
  :112 reads the origin pathname through routing and gets task source. Calls
  :119–138 discover an external reader for the competing version. Concurrent
  conflict-input retention work appeared during this review and is preserved;
  no competing conflict-resolution protocol was added here.
- **Required waits and validation:** F3(2):123–125 waits for a real test run and
  receives terminal failure evidence. The later :148–149 poll of that completed
  process is unnecessary, but the existing result already says it exited and
  includes the test failure summary. This does not justify reviving handles or
  hiding completion. Later waiting and validation address actual overlapping
  source changes; required work must remain.
- **Architecture and telemetry:** F3's owner-held code-mode waits let JavaScript
  finish its awaited work without an outer model wait call. Its 55 internally
  drained observations do not prove 55 saved model decisions. No compaction,
  budget eviction, no-progress directive, or repeated completion generation is
  recorded. The transport retry is correctness work. No scheduler threshold or
  telemetry counter is changed.

The review followed turn continuation/completion and request retry in
`session/turn.rs`, sampling context preparation, code-mode execute/wait ownership,
registry preparation and preflight, unified command admission, transaction
routing/capture, and artifact recovery. Existing snapshot reuse and structured
JavaScript result repairs remain in place. There is no evidence here supporting
another blanket generation suppression, read cache, or polling ban.

## Implemented change

Path: public semantic/validation arguments → `prepare_invocation` → registered
`exec_command` → code-mode preflight and hooks → command handler → workspace
worker. Existing direct-dispatch tests used `ToolCallSource::Direct`, bypassing
the preflight exercised by these logs.

Both helpers now emit canonical command arguments. `program` selects direct argv
execution without a redundant `kind`. An omitted or explicitly selected primary
environment is represented by omission; a secondary selection is forwarded.
The validation worker still receives its original `force_fresh`, checks, and
full-suite authorization. The outer worker invocation is neither an rg-miss
cache candidate, an immutable Git-show cache candidate, nor an implicit-patch
failure candidate; removing its unsupported freshness field does not introduce
result reuse. Worker-owned fingerprinting and freshness remain authoritative.

An overlapping registry compatibility candidate was removed during concurrent
integration: canonical helper arguments make a schema extension unnecessary.
The helper expansion now obeys the advertised shape, including explicit primary
selection, without bypassing preflight. Native approval,
sandbox, cancellation, process ownership, source snapshot validation, and
post-integration verification remain unchanged.

Regression tests execute the registry path with an instrumented command handler,
not merely schema/counter assertions. They require dispatch for both helpers,
with and without pre-hook preparation, and with omitted/primary/secondary
environment selection. Negative cases make command execution panic if an invalid
position, action, or unknown environment reaches it. Existing transaction tests
verify immutable inputs and unchanged origin files. A Windows regression carries
a long-path source through capture, validation and reconciliation, and rejects a
modified validation snapshot.

## Validation

**25 distinct tests passed**, with no failures or ignored selected tests:

- Three focused regressions: command dispatch for both helpers, invalid-request
  rejection before execution, and long-path snapshot/validation/reconciliation
  (`focused-built-binary.log`).
- Fourteen existing helper, public command-schema and workspace transaction
  tests, excluding the three already-passing tests
  (`existing-built-binary.log`). These also exercise the concurrently added
  retained conflict inputs, negative conflict publication, exact routing, and
  validation-snapshot invalidation.
- Three existing registry preflight tests: invalid hook rewrites, prescriptive
  command-boundary errors, and ordinary dispatch without an extra preflight
  (`preflight-existing.log`).
- All five existing validation-worker tests, built with
  `cargo test --locked -p codex-workspace-tools --lib validation::tests::`.
  The real Cargo fixtures verify pass reuse, dependency-content and file-addition
  invalidation, failure retention, forced freshness invalidating an older pass,
  reverse-dependent planning, and rejection of broad/output-override checks
  (`workspace-validation-existing.log`).

The repository runner completed its core test inventory build in 10m 05s, then
its queued execution build was terminated with status 4294967295 before running
tests. That compilation and queue work is retained in `focused.log`, not counted
as passing tests or omitted from the local work record. The first twenty tests
above were then executed from the successfully built core test executable,
`codex_core-a27b9c8d51f699e4.exe`, containing the regressions. It was built after
the touched helper source files were last modified. The direct invocations used
the same Windows test environment as the runner: inherited `CODEX_*` values
cleared, an 8 MiB Rust thread stack, the core crate working directory, and serial
test execution. This does not claim a later full-worktree rebuild.

Pinned rustfmt checks passed for both changed helper files; whitespace checks
passed for the touched files. Final source review confirms argv worker dispatch
still goes through normal command policy, explicitly selected secondary
environments are retained, and freshness remains in the worker request. The
instrumented dispatch workflow and real validation fixtures reproduce the
relevant boundary without a paid model benchmark rerun.

No installed binary is replaced and Desktop is not restarted.
