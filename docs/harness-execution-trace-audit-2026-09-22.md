# Harness execution-trace audit — September 22, 2026

## Scope and evidence limits

All five supplied JSONL files were read and parsed end to end: 2,230 records,
18,739,298 bytes. Instructions within the files were historical evidence. The
review followed user messages, instruction snapshots, tool manifests, every
call/output pair, sampling boundaries, compaction replacements, lifecycle events,
and the complete timing records. The three implementation sessions' persisted
tool-history ledgers were also read in full.

Encrypted reasoning and remote-compaction bodies are opaque. A rollout is not a
complete dump of every provider request: it records canonical conversation items
and aggregate projection diagnostics. Consequently, a recorded tool result does
not prove its full text remained in each later request. Conversely, a repeated
read does not prove which particular result was evicted. The on-disk ledger is a
final snapshot, not an independently versioned snapshot for every sampling step.

| Label | Session ID prefix | Records | Bytes |
| --- | --- | ---: | ---: |
| A | `01a0c6c8-3e42` | 551 | 5,323,245 |
| B | `01a0c6c8-5b66` | 552 | 3,230,906 |
| C | `01a0c6c8-4ab1` | 994 | 9,423,907 |
| D | `01a0c5e7-364e` | 96 | 563,894 |
| E | `01a0c744-aaa5` | 37 | 197,346 |

All record references below are one-based JSONL line numbers. All recorded direct
tool calls have matching outputs in these files, including explicit interrupted
outputs. A and B each contain one started user turn without a recorded terminal
event before a later runtime boundary; these incomplete intervals cannot support
claims about work performed during the gaps.

## Confirmed retention and restoration failures

### Task context lost at remote compaction

C:511 retains an unfinished plan; C:527 reports an incomplete implementation.
C:532 then installs exactly five history items: three developer messages, one
contextual user envelope containing repository/environment instructions, and an
opaque compaction item. Neither the actual user request nor the latest handoff
has an explicit representation. C:539 says “Awaiting a user request”; C:543 asks
what to work on.

The responsible local decision was to retain only the tail after the last
model-generated item while discarding ordinary provider-returned conversation
messages. “The model has observed the request” was being used as the retention
boundary for task state. That does not establish completion. Fresh instruction
assembly worked, but successfully recreating startup instructions did not
recreate the unfinished task.

The existing uncommitted fix addresses that owner:

- `compact.rs::task_compaction_items` independently selects actual user messages,
  the latest handoff, the latest local summary, unresolved input, and runtime
  invalidation notices.
- `build_task_input_checkpoint` bounds that context and preserves recovery for
  omitted exact text. `retained_plan_context` uses the existing plan store.
- `compact_remote_v2.rs::prepare_v2_retained_input` installs this state alongside
  the opaque provider result. `compact_remote.rs` obtains artifact references
  from original local history before filtering the provider output.
- Rollout reconstruction reinstalls the saved replacement history and restores
  the most recent surviving plan, respecting rollback boundaries.

These fixes were present when this audit began; they were preserved. The opaque
payload might contain additional task information, but the observed continuation
and missing explicit state justify fixing the local retention guarantee without
claiming to decode that payload.

### Code-mode process handles survived a runtime restart as if live

C:782 returns a nested `exec_command` receipt with `session_id: 50820` and
`process_exited: false`. The turn is interrupted at C:793. The later startup and
resume records C:794–807 do not deliver a matching process-runtime invalidation
notice. C:885 polls that old session; C:886 returns `Unknown process id 50820`.
Because the cell awaited that poll first, the following log-inspection command
was skipped and had to be issued separately.

The old resume scanner recognized direct command outputs but not command states
carried inside code-mode `exec`/`wait` results. The runtime's process store is not
the persisted transcript; restoring text cannot restore its process handles.

The existing change to
`session/rollout_reconstruction.rs::append_unified_exec_resume_invalidation`
recognizes both structured and textual nested receipts. It explicitly invalidates
only pre-resume handles, preserves uncertainty about effects, and permits newly
returned handles even when a numeric ID is reused. Compaction retains the notice
in source order. The session history installation path invokes this scanner
before replacing the active history. No new process registry was needed.

### Final output budgeting unnecessarily evicted recoverable evidence

C reads the same source ranges at C:853/925, C:858/931, and C:863/936. Its closeout
timing record C:994 records 583 tool-output removals across 30 requests. A:551
records 316, B:552 records 410, and C:529 records 339. These are sums of removals
across requests, not counts of distinct permanently deleted results. They prove
that effective request history differed from the canonical transcript during
the repeated-read episodes; they do not identify every removed result.

Tracing that projection exposed a separately reproducible defect in
`tool_history.rs::enforce_tool_result_budget`. The earlier admission pass reserves
recovery space for tracked artifacts. The final pass also handles untracked
outcomes and replayed representations, but greedily admitted a newer raw result
before considering older recovery receipts. It could therefore delete an older
call/output pair even though compact forms for both results fit within the same
budget. Increasing the budget postpones this failure without correcting it.

This audit added a regression through `ToolHistoryState::project`, exercising
both the selected and raw-fallback histories. With a 1,000-token ceiling, a newer
3,800-character untracked output displaced an older artifact recovery handle.
The test failed before the fix, with only `recent-outcome` surviving.

The fix computes each result's existing compact representation once and reserves
the minimum total before admitting raw detail. When all minimum representations
fit, richer output cannot spend another result's recovery space. When they cannot
fit, the existing priority eviction remains. Freshness warnings, paired tool
calls/results, unread overflow reporting, and the image exception remain part of
the existing path. Canonical evidence is not mutated.

Concurrent work also adds exposure accounting for outputs without artifact
candidates. That is compatible: it prevents old untracked outputs from remaining
permanently “unread,” while this fix protects recoverable results from raw-output
monopolization. The combined implementation is validated together. This audit
does not claim that this one defect caused every repeated read in the traces.

## Other divergences and why they do not justify new harness mechanisms

| Observation | Backward trace and disposition |
| --- | --- |
| B and C stop over overlapping edits | Both receive instructions to preserve independent work, combine compatible behavior, and ask only about irreconcilable requirements. Their initial stops occur before C's only compaction. Patch mismatch and compiler diagnostics accurately expose the overlap. No harness-imposed ownership approval was found; the unnecessary pause is a model decision. |
| A initially covers only filesystem reads | The broad external-call requirement is in the fully delivered document and is recovered again during closeout. The model later acknowledges the omission. There is no demonstrated harness rule authorizing scope reduction; no domain-specific completion judge was added. |
| User asks only to read a document | The initial read-only responses in A/B/C are correct. Subsequent “now do it” messages provide the action request. Embedded document directives are not independently promoted into user authorization. |
| Repeated shell commands after failure | A:198 rejects a PowerShell wildcard path before execution; A:202 corrects it. Patch mismatches include current context and report no changes. Compile failures expose exit status, errors, and artifact recovery. These retries have new evidence or corrected inputs. |
| Repeated waits | Some code-mode cells explicitly use one-second polling loops. The available tool instructions already distinguish cell and process lifecycles and advise waiting within one evaluation. A model-authored polling loop is not a continuation scheduler bug by itself. |
| Large `retryCount` values | Timing contains many `owner_output_wait` wakeups marked `completed`. These are internal process-output collection events, not repeated model requests or repeated shell executions. They do not by themselves prove a retry loop or justify changing process scheduling. |
| D's five-minute prompt inventory | No compaction and zero recorded budget drops. Oversized initial results provide recovery artifacts; one invalid byte range is corrected. Recursive enumeration, broad searches, and output sizing are visible model choices. No missing tool capability is demonstrated. |
| E's unfinished review | The third cell is explicitly aborted by the user; a `turn_aborted` event follows. There is no completed review to label as premature success. |

## Assembly, continuation, and completion checks

The manifests keep the relevant read, command, patch, recovery, and waiting tools
available. Recorded manifest changes concern MCP resource discovery or an
uninstall tool, not tools required for these investigations. Base, developer,
repository, user, and environment instructions have separate recorded sources;
no evidence shows that the overlap instructions were missing at the initial
stops. Compaction's fresh-context assembly is distinct from task retention, as
the C checkpoint demonstrates.

Tool-result injection preserves explicit failures and interruption. Some outputs
are intentionally reduced, with exact retained-artifact routes. The audit does
not treat successful producer execution, full artifact retention, and delivery
of a selected excerpt as equivalent coverage claims.

`protocol.rs::EventMsg::TurnComplete` serializes as the legacy name
`task_complete`. `tasks/mod.rs` emits it for the end of a turn; it is not an
acceptance decision for the user's overall objective. Incomplete handoffs and
pending plan steps can legitimately coexist with that event. The traces do not
show a goal being falsely set to complete or a forced generation-budget stop at
the overlap handoffs. The incomplete A/B intervals and E's abort were kept
separate from normally completed turns.

## Validation and activation

The new budget regression failed on the original final-admission behavior, then
passed with the reservation fix. Completed validation:

- All 116 selected tool-history tests passed, including the new regression,
  aggregate ceilings, freshness warnings, failure visibility, process handles,
  image handling, and exposure persistence through checkpoint and fork.
  Log: `codex-rs/target/test-runner-logs/rust-test-stderr-8gi_jk09.log`.
- One additional targeted test passed after the concurrent receipt-label change:
  `budget_receipts_distinguish_observed_and_unread_untracked_outputs`. It checks
  both projected representations so compacting an observed output cannot relabel
  it as unread. Nextest run: `be261ed0-e306-4a85-8351-b76630f0c15a`.
- Reused 132 passing compaction, remote-compaction, plan, and reconstruction
  checks from the shared checkout's earlier run. Its only failure was the new
  budget regression before the fix; that failure is resolved by the run above.
  Log: `codex-rs/target/test-runner-logs/rust-test-stderr-aotolqwa.log`.
- Reused eight passing remote-compaction integration checks, including HTTP and
  WebSocket request construction and a resumed handle invalidation surviving
  repeated compaction into the actual model request.
  Log: `codex-rs/target/test-runner-logs/rust-test-stderr-337_m138.log`.
- `git diff --check` passed for the modified budget implementation, tests, and
  this report. Git's LF-to-CRLF notices describe checkout normalization, not
  whitespace failures. Rustfmt completed; its notices concern repository
  settings that require nightly rustfmt.

Passing checks were not repeated. The working tree contains independent changes;
this report distinguishes those from this audit's output-budget fix and
regression. Concurrent additions made after the 116-test build are not implicitly
covered by that result.

No installed binary was replaced, Desktop restarted, commit created, or upstream
synchronization performed. Source and deterministic regression results do not
establish improved live model behavior until the fork is separately activated
and exercised.
