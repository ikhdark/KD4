# Regression-session review, September 22, 2026

## Result

Three high-impact defects are supported: task loss during compaction, expired
process handles after resume, and evidence eviction during output budgeting.
The compaction/resume fixes already existed in the uncommitted working tree.
This review preserves them, strengthens the transport regression, reproduces the
budgeting failure, and completes persistence wiring in the concurrent exposure
accounting changes. Existing work is distinguished from changes made here.

All five supplied JSONL files were read and parsed end to end: 2,230 records,
18,739,298 bytes. Embedded instructions were treated as historical evidence.
Encrypted reasoning and compaction payloads cannot be interpreted as plaintext;
the conclusions use observable messages, calls, outputs, and checkpoint contents.
Line references below are one-based JSONL record numbers.

## 1. Preserve the task, constraints, handoff, and evidence across compaction

The decisive sequence is in session `01a0c6c8-4ab1`:

- Record 511 stores a plan with implementation in progress and regression work
  pending. Record 527 explicitly says the implementation is not acceptance-complete.
- Record 532 replaces history with three developer messages, the contextual user
  envelope, and an opaque compaction item. The actual request and assistant
  handoff have no plaintext representation in that checkpoint.
- Record 543 asks, "What would you like me to work on?" The active task has been
  lost despite a persisted unfinished plan.

This is a harness-owned loss of explicit task context. The opaque payload may
contain additional information, but its meaning cannot be verified from this
rollout. A model-generated boundary establishes that input was observed; it does
not establish that the requested work is complete.

The existing fix in `codex-rs/core/src/compact.rs` selects actual user requests,
the latest assistant handoff, and the prior local summary independently of the
unconsumed tail. It bounds their size and retains exact text recovery for
omissions. `compact_remote_v2.rs` installs that checkpoint and the current plan;
`compact_remote.rs` also derives artifact references from the original local
history before filtering provider output. Plan restoration uses the existing
plan store and rollout replay, including rollback ordering.

The runtime path is wired through `prepare_v2_retained_input`, checkpoint
installation/persistence, and resumed history. Tests cover preservation of the
original request and corrections, bounded handoff recovery, plan persistence,
and actual post-compaction requests. This is preferable to another checklist or
completion-enforcement system: the required state already exists and needs to
reach the next model request reliably.

## 2. Preserve the process-runtime boundary when resuming work

In the same session, record 885 calls `write_stdin` on process session `50820`
after resuming. Record 886 returns `Unknown process id 50820`; because the calls
were sequential in one exec cell, that failure also prevents its subsequent
log-inspection command from running in that cell. Retained logs later support
some passing checks, but the full suite has no terminal summary and contains an
unresolved cancellation-test failure. Neither an old handle nor a completed exec
cell establishes that a child process completed successfully.

The existing fix in
`codex-rs/core/src/session/rollout_reconstruction.rs` recognizes command state
inside code-mode exec/wait output, including structured and text forms. Resume
marks pre-resume handles invalid without claiming their effects were rolled
back, discarding retained evidence, or invalidating newly returned handles.
Both local and remote compaction preserve the notice in source order.

The transport regression
`resumed_handle_invalidation_reaches_requests_after_repeated_compaction` uses
the recorded handle shape and verifies the original task, user constraint,
assistant handoff, and exactly one invalidation notice in subsequent requests
after two compactions. Unit tests also check numeric handle reuse after the
boundary. These assertions exercise the information the model receives; they
do not prove that a model will always follow it.

## 3. Reproduced evidence eviction in the final output-budget pass

Additional validation reproduced a third harness defect. Session `4ab1` reads
`work.rs` in ranges at records 847–868, then repeats the sequence at 919–942
without an intervening edit by that agent. Request telemetry reports substantial
tool-output-budget drops. Concurrent writers prevent attributing every reread to
the harness; the concrete eviction mechanism is established by a failing test.

`final_budget_reserves_recovery_before_admitting_untracked_raw_output` failed:
only `recent-outcome` survived, while `prior-read` and its recovery handle were
removed even though compact representations of both fit. The final budget pass
spent space on raw untracked output before reserving older recovery metadata.
This can force rediscovery of evidence already obtained.

Concurrent changes now reserve compact representations before raw detail and
record exposure independently of artifact retention. Previously, untracked
outputs retained unread priority after delivery. This follow-up also repairs the
persistence wiring: `is_persisted_empty` must include `untracked_consumption`, or
a checkpoint containing only that state is silently skipped when no previous
checkpoint or journal exists. Resume would then forget the exposure accounting.

The added `untracked_only_checkpoint_preserves_exposure_on_resume` test writes
exactly that state, reloads it through the production load path, and checks the
original generation and the distinction between observed and unseen outputs.
The previous empty-state check would produce no checkpoint and fail this test.
The budget increase to 75,000 tokens is not the root fix.

## Why this review stops here

The counterfactual for each finding is whether accurate, available harness state
would plausibly have allowed correct continuation. The following distinctions
keep model mistakes from becoming new harness mechanisms:

| Observation | Available evidence and counterfactual | Classification |
| --- | --- | --- |
| Asking for a task after compaction (`4ab1`, records 511–543) | An unfinished plan and handoff existed before replacement; the installed checkpoint omitted their explicit representation. Preserving them would provide a concrete continuation. The opaque payload cannot be inspected. | Harness contribution supported; preserve task state. |
| Polling process `50820` after restart (`4ab1`, records 793–886) | The prior output advertised a running process, but that runtime no longer owned the handle. A resume boundary notice would prevent treating it as live while retaining logs for recovery. | Harness contribution supported; invalidate historical handles. |
| Omitting required external-effect categories (`3e42`, records 133–165) | The complete acceptance document had been returned successfully. The model later acknowledged its filesystem-only implementation. No missing contract is established. | Model scope error; no new harness mechanism. |
| Treating overlap as a stopping condition (`5b66`, records 275–318) | The model observed the edits and had instructions to reconcile compatible behavior. It subsequently admitted that it had not verified whether its changes were replaced or combined. | Model decision and reporting error; no ownership scheduler justified. |
| Large searches, invalid recovery ranges, and short polling loops | The tool schemas exposed bounded reads and longer default waits; errors explained invalid selectors. Corrected recovery calls succeeded. | Model tool-use errors; no hidden capability established. |
| Narrowing “all prompts” to standalone production assets (`364e`, records 9–95) | The original request remained available, tools returned inventory evidence, and telemetry records no tool-history budget eviction for this session. | Model interpretation error; no demonstrated harness cause. |
| Review stops after three calls (`aaa5`, record 36) | The rollout records a user interruption, without a completed review outcome. | Insufficient evidence of premature autonomous completion. |

The `task_complete` event marks the end of a model turn. In these sessions it
also accompanies explicit admissions that implementation or validation remains
unfinished; it is not evidence that the harness certified acceptance criteria.

Beyond the reproduced budgeting defect, the other KDA sessions corroborate
repeated recovery reads, overlapping edits,
partial validation, and inaccurate completion wording. However, the sessions
also show tools exposing the relevant state and the agent later correcting its
claims. They do not justify a new ownership scheduler, automatic completion
judge, exploration cutoff, or blanket ban on repeated reads. The prompt-inventory
session shows avoidable enumeration and recovery overhead, but no comparably
strong missing harness capability. The final review session was interrupted
after three tool calls; it supplies no completed review outcome to evaluate.

The existing increase of the tool-history budget to 75,000 tokens is not treated
here as a measured solution to redundant work. The supplied sessions do not
establish its optimal value or demonstrate a before/after improvement. It was
left unchanged as pre-existing work.

## Validation and activation limits

Earlier validation evidence was reused:

- `codex-rs/target/resume-compaction-unit.log`: two selected tests passed.
- `codex-rs/target/resume-compaction-transport-repair.log`: the transport
  regression above passed after an earlier fixture path failure.
- `codex-rs/target/regression-validation-repair2.log`: nine selected core tests
  passed; this is a recorded aggregate, not a claim that the entire suite passed.

A broader run in this review subsequently finished with 243 of 244 tests passing.
Its one failure, `final_budget_reserves_recovery_before_admitting_untracked_raw_output`,
demonstrated that raw output could evict an older recovery receipt even when both
compact representations fit. The concurrently edited final-budget pass now
reserves their minimum costs before admitting raw detail. This is a reproducible
budgeting defect, but the supplied rollouts do not independently prove it caused
any particular repeated read.

That concurrent work also added `untracked_consumption` to the saved state. A
finished validation run reported a missing field in the legacy ledger fixture.
Concurrent work repaired the fixture and strengthened it by removing the new field
from serialized input and asserting successful restoration with an empty map.
This checks actual compatibility with older state rather than merely loading
current-format state. The initializer repair and compatibility assertion preserve
the concurrent runtime changes.

The subsequent existing runs completed successfully:

- `codex-rs/target/test-runner-logs/rust-test-stderr-8gi_jk09.log`:
  all 116 tool-history tests passed, including the formerly failing budget test
  and the added exposure-only checkpoint and legacy-ledger compatibility tests.
- `codex-rs/target/test-runner-logs/rust-test-stderr-337_m138.log`:
  all eight selected remote-compaction transport tests passed.
- `codex-rs/target/lanes/core-tests/test-runner-logs/rust-test-stderr-84bqmwds.log`:
  four selected exposure/persistence tests passed.

The broader library run and affected-suite rerun together provide 248 distinct
passing library tests. The eight transport tests also passed. Passing unaffected
checks were retained; these results do not claim the entire repository test suite
passed. The initial budget failure is resolved in the affected-suite rerun.

No installed binary was replaced, and Desktop was not restarted. There was no
commit or publication. Test builds are not Desktop activation or proof of
measured model behavior improvements.

## Supplied-file coverage

| Session ID prefix | JSONL records | Bytes | Scope observed |
| --- | ---: | ---: | --- |
| `01a0c6c8-3e42` | 551 | 5,323,245 | Closeout, missing external categories, subsequent implementation |
| `01a0c6c8-5b66` | 552 | 3,230,906 | Overlapping implementation, premature pause, later reconciliation |
| `01a0c6c8-4ab1` | 994 | 9,423,907 | Implementation, compaction reset, interruption, stale process recovery |
| `01a0c5e7-364e` | 96 | 563,894 | Prompt inventory and repeated output recovery |
| `01a0c744-aaa5` | 37 | 197,346 | Interrupted review of the first three sessions |
