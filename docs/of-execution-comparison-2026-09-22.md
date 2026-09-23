# O/F execution comparison and repairs

## Evidence and accounting

Inputs are `o1 (1).jsonl`, `o1 (2).jsonl`, `f1 (1).jsonl`, and
`f1 (2).jsonl`, supplied from `New folder (4)`. References below use one-based
JSONL record numbers. All 1,997 records were parsed. Historical instructions
in those records are evidence, not instructions for this repair.

| Combined measurement | O | F |
| --- | ---: | ---: |
| Records | 864 | 1,133 |
| Model work | 110 recorded usage completions | 162 logical generations, 163 physical requests |
| Input tokens | 14,267,034 | 12,681,019 |
| Cached input tokens | 13,918,720 (97.56%) | 11,713,664 (92.37%) |
| Uncached input tokens | 348,314 | 967,355 |
| Output tokens | 68,127 | 88,661 |
| Reasoning output tokens (included in output) | 24,895 | 18,174 |
| Outer tool calls | 106 | 156 |

F's timing records additionally report 372 tool calls, including nested calls,
and 16 artifact-recovery calls. O has no equivalent nested timing ledger, so
372 is not compared with O's 106 outer calls. Static occurrences of calls in
JavaScript are not execution counts: loops and aborted scripts differ.

F2:398 is an interrupted, resource-consuming turn: 45 generations and 105 tool
calls over 1,160,492 ms. It is included, along with the subsequent instruction
and resumed implementation turns. F1 includes one physical request retry.
Token totals use each session's final cumulative usage rather than summing the
repeated cumulative records. Missing provider usage for any partial stream
cannot be reconstructed from these rollouts.

These are not four repetitions against an immutable repository. The proposals
and implementation scopes differ as the repository evolves: F1's first answer
includes closure capture, shared-state identity, and trait-law work, whereas
O1 proposes iterator cursor and collection transitions. Concurrent edits cause
real compilation and provenance failures, and F2 receives a new prohibition
on full-suite validation. More model calls or lower cache ratios therefore do
not establish a regression by themselves.

## Confirmed repair targets

### Completed command results became script exceptions

F1:222,363,400,420,449 and F2:294,373,452,502,531 show nonzero process
results converted into `Script failed`, even though the process result has an
explicit exit code and retained diagnostic output. In F2:444/452 the script
awaits and prints process results in a loop; rejection bypasses its normal
result handling. The catch-free scripts cannot inspect the result or execute
later JavaScript. The error path also embeds the serialized result in both
retained nested evidence and a script exception, enlarging diagnostics before
the next model request. Necessary compile/test repairs remain necessary;
the additional JavaScript exception is not necessary to report the failure.

Path: command or polling handler -> `ExecCommandToolOutput` -> registry result
and hook/projection wrappers -> `code_mode::call_nested_tool` -> broker -> JS
promise -> cell terminal result -> result formatting -> sampling signals and
ordinary tool continuation. The adapter unconditionally rejected any output
whose logging outcome was Failure/TimedOut. That conflated a successfully
observed failing process with a failed tool invocation.

The repair lets the typed command result retain its normal JavaScript return
contract, independently of its failure classification. Dispatch errors,
cancelled operations, failed patches, and other tools' exception contracts
remain unchanged. Nested failure evidence and continuation signals remain
recorded even if JavaScript handles the process failure successfully.

### Selected diagnostics were truncated a second time

F2:452 contains a failure-focused shell summary followed by another truncation
warning; F2:457,463,469,475,481 then recover the same diagnostic artifact in
successive cells. F1:449/454 similarly recovers a failed validation result.
Not every recovery is avoidable: exact large assertion data may require an
artifact read. But the current implementation separately reproduces loss of
compact diagnostic labels that all fit together within the caller's budget.

Path: raw process output -> command model-output limits ->
`summarize_shell_output_for_model` -> selected diagnostic/context/status lines ->
formatted output truncation -> code-mode packet formatting -> artifact recovery.
The summary uses the applied token budget to decide whether to summarize, but
allocates selected line detail against a fixed 32 KiB ceiling. A second,
smaller token ceiling can discard the middle of that already-selected summary.

The repair budgets the rendered selection itself, including metadata, line
numbers and omission markers, before downstream projection. Detail within
oversized lines yields space to the other selected diagnostic regions. Exact
raw artifacts, explicit omission reporting, source order, read-only source
output behavior, and downstream hard ceilings remain intact.

## Broad inspection and exclusions

| Area | Finding and classification |
| --- | --- |
| Scheduling and turn completion | `session/turn.rs` derives follow-up from model tool work and pending input, then applies completion/owner state. No automatic empty continuation or false objective-completion decision was established in these traces. `task_complete` ends a turn, not proof of repository acceptance. |
| Progress/control telemetry | F reports zero no-progress directives and zero budget-dropped tool results in these turns. Purpose labels and internal drain counts do not independently establish redundant generations. Failure signals must survive the command-result repair. |
| Wait and polling | F's eleven outer waits follow yielded cells; internal `write_stdin` loops are real process observation. O also repeats polling cells. A wait count is not proof of unnecessary model work. Explicit yields, running handles, cancellation and required terminal outcomes retain their existing paths. |
| Request construction and cache | Inspected prompt preparation, exposure and history anchoring. Active worktree changes already address historical evidence invalidation and cache-prefix stability. Those are preserved and are not claimed as repairs here. Lower F cache reuse alone does not identify an additional cache defect. |
| Compaction/state reconstruction | No compaction records occur in these four logs. Existing task checkpoint, resume invalidation and plan restoration mechanisms cover previously audited failures; no duplicate mechanism is added. |
| Tool-result recovery | Current registry code already returns the handler's raw result to JavaScript rather than substituting the model projection. Source artifact recovery is available. The new repair addresses command rejection and diagnostic re-truncation, not all repeated inspection. |
| Retries and concurrent edits | Patch mismatches, changed source, stale analyzer provenance and compile failures provide new evidence requiring some reinspection or validation. Patch retries/transactions and semantic-context tools are actively being changed in this worktree and are outside this repair. |
| Interruption | F2's interrupted work is counted, and the later user direction changes validation scope. Inspecting affected uncertain execution after interruption is correctness work. No repeated process execution attributable to a new interruption bug was established. |

## Validation

260 distinct checks passed across the focused regressions and the affected
`tools::code_mode`, `tools::shell_output_summary`, `tools::context::tests`, and
`tools::registry::tests` modules. Passing checks were excluded from subsequent
runs. One existing test,
`registered_code_cells_persist_ordered_initial_and_terminal_traces`, failed on
its first attempt and passed on the runner's automatic retry. This is a
validation limitation, not a claimed repair of trace/wait scheduling.

The execution regressions prove:

- `command_failure_remains_inspectable_without_another_cell`: real JavaScript
  inspects nonzero command and polling results and actually executes its next
  tool in the same cell. The normal continuation consumer still selects failure
  diagnosis, without treating an ordinary command failure as non-retryable.
- `command_failure_state_survives_explicit_yield_before_recovery`: the command
  result survives an explicit yield and resumed JavaScript executes recovery.
- `command_dispatch_errors_still_stop_dependent_work`: dispatch rejection still
  fails the cell and prevents the dependent tool from executing.
- `caller_budget_preserves_each_selected_failure_without_retruncation`: actual
  command projection preserves three separated failure labels and final status
  within both tested caller budgets, without a second truncation. Existing
  checks also cover required raw recovery, source reads, tiny budgets, byte
  ceilings, cancellation, waits, blocked outcomes and projection wrappers.

Run evidence in `codex-rs/target/lanes/core-tests-2/test-runner-logs`:

- `rust-test-stderr-ynusc3e9.log`: 4 passes and one failed regression assertion.
  The assertion incorrectly required a non-retryable suppression fingerprint;
  it was corrected to exercise normal outer-call bookkeeping and assert the
  failure-diagnosis and retry contracts instead.
- `rust-test-stderr-9zaz46w4.log`: 253 passes (including that corrected regression
  and one flaky pass), three stale expectation failures. Two byte-ceiling tests
  now exercise that ceiling independently of a tighter caller token budget;
  their exact source-count and gap assertions remain. The workspace-dependency
  test now expects the existing recursive working-directory scope for Git
  status while retaining the earlier observed file dependency.
- Nextest run `9a01c7e8-cd9f-4017-951b-36d905fe3839`: those three corrected checks
  passed using the local profile, with retries disabled.

Earlier builds consumed resources but did not execute tests: concurrent edits
caused compiler failures, builds waited on shared locks, and a helper build
ended with exit code 4294967295 without a compiler diagnostic. An incorrectly
resolved validation target directory was also detected and its build stopped.
These attempts are not counted as passes. Final unit validation used the
existing Rust runner and a temporary `target/of-validation.toml` declaring the
same core library target without helper executables: the selected modules use
pure data or the in-process JavaScript provider. Repository runner configuration
was not changed for this validation.

No installed binary was replaced or Desktop restarted. These deterministic
execution tests establish removal of the identified failure paths; they do not
predict a particular live token saving or constitute a full workspace test run.
