# Verification of New folder (5), documents 11–15

All five supplied documents were read completely: 423, 426, 441, 434, and
397 lines respectively (2,121 lines total). Their imperative recommendations
were treated as claims to investigate, not as instructions. No other agents
were contacted. Verification concerns this local checkout; no upstream
synchronization, deployment, Desktop binary replacement, or restart was done.

The checkout already contained extensive edits. Existing repairs are identified
separately below, and compatible changes were preserved. Predicted task-success,
token, and latency improvements are not reported as measured results.

## Claim dispositions

| Report / claim | Finding and disposition |
|---|---|
| 11.1 Search failures lose their explanation | The native incomplete result has no diagnostic field, but the current checkout also carries `ToolCallCompletion.failure_detail` through ordered recording. The bounded search-error companion and its tests already exist; no second mechanism added. |
| 11.2, 13.1b, 14.3 Promptless hook blocks become success | Already addressed: `CompletionStopHookReport.should_block` survives rendering and the caller emits a status-affecting error and aborts. |
| 11.3 Every denial terminates the task | Confirmed. Call-local `DeniedToModel` now leaves permitted continuation available. A separate `RequiredOperationBlocked` preserves terminal behavior when an assignment has been revoked; explicit required-operation outcomes and fatal errors remain terminal. Both direct and code-mode calls use the shared classifier. |
| 11.4 Plugin telemetry delays sampling | Confirmed. Metadata enrichment now runs as bounded best-effort blocking work (eight admitted jobs), without a turn-completion join. Semantic planning effects remain ordered. |
| 11.5 Workspace baseline map serializes independent captures | Already addressed: the map lock obtains a per-workspace cell and releases before capture/watch awaits. Same-workspace capture coalescing remains. |
| 12.1 Notification and listener bookkeeping can wait indefinitely | Confirmed. Ordinary notification admission now has one fanout deadline; failed routes are invalidated. Listener resolution bounds command admission and acknowledgement under one deadline and cancels the failed listener route. Human approval decisions are not timed out. |
| 12.2 Dynamic calls broadcast and replay executable work | Confirmed. `thread/start` registers the dynamic-tool owner; dispatch selects only that registered eligible connection. Legacy/cold-resumed registrations without an owner require exactly one eligible client; ambiguous ownership fails closed. Pending executable requests are not subscription-replayed, including to the original client, because the protocol has no execution deduplication contract. Owner state is removed on thread unload. |
| 12.3 Invalid grant paths interrupt the turn | Confirmed. Invalid conversion rejects the entire grant with empty, turn-scoped permissions. The original call receives its normal response. No global interrupt or turn-summary error is emitted. Existing grant intersection and turn-transition cancellation remain. |
| 12.4 A settled request can remain stuck in fanout | Confirmed. Callback lifetime owns a settlement guard. Response, error, cancellation, and final-recipient removal wake initial capacity admission immediately; the synchronous publication fence remains. |
| 12.5 Responses are decoded for analytics and again for consumers | The duplicate conversion exists. This is a measurement-dependent optimization, not a demonstrated protocol failure: consumers already reject malformed replies. A typed callback migration changes every consumer and its error contract. No unmeasured interface redesign added. |
| 13.1a Fresh message IDs defeat stop-repair equality | Already addressed: comparison uses continuation feedback plus settled revisions and evidence, not the newly constructed message identity. |
| 13.2, 14.1 MCP conversion drops independent evidence and cleartext error status | Confirmed. Structured JSON now accompanies unique text/images. Only text that parses to the identical structured JSON value is removed as a mirror. Cleartext errors include an explicit marker; ordinary text becomes plain output text. Encrypted handling remains intact and private content `_meta` is excluded from fallback text. |
| 13.3, 14.2 Detailed diagnostics gate completion/recovery | The awaits exist. They also complete attempt accounting used in final timing snapshots. The reports provide no measured delay or replacement contract for late accounting. Retained these semantics; no claim of performance benefit from moving them. |
| 13.4 Receipt overlap always destroys usable WebSocket inheritance | Outdated as stated: the current fallback first tries incremental reuse against canonical history and retains `previous_response_id` when compatible. Avoiding construction of the discarded projection remains a measurement-dependent optimization. |
| 13.5 Hashing should use a borrowed comparison first | Full candidate hashing exists, but the proposed two-pass path requires parity and workload measurements. No demonstrated failure or measured improvement warrants adding it. |
| 14.4 Admission measures raw attachments instead of rendered content | Already addressed: `estimate_pending_tokens` invokes the same `ResponseInputItem` rendering used for insertion; local images use a metadata-only estimate. No second prepared-input abstraction added. |
| 14.5 Sequential quotas gratuitously truncate large-first attachments | Confirmed. Demand is measured across selected paths before allocation. Small demands are satisfied first and remaining large paths share the remainder equally. Rendering retains original order, group limits, and the existing single-path margin. |
| 15.1 Late wait endings attach to the wrong turn | The primary retained-owner repair already exists. Fixed its remaining ambiguity case: multiple historical owners now suppress the unscoped legacy event rather than falling through to the current turn. Rollback also remembers removed wait identities so late ends cannot resurrect them; reset clears that state. Unrelated end-only legacy records still use the existing fallback. |
| 15.2 Raw hook prompts duplicate canonical items | Confirmed. Raw reconstruction now uses identity-based upsert. Both representation orders, repeated raw records, and distinct IDs are covered. |
| 15.3 No-op upserts emit redundant snapshots | Source observation confirmed; suppression depends on consumer recovery semantics and measured comparison/copy costs. No change to that contract. |
| 15.4 Completed-turn indexes are discarded | Source observation confirmed. Retention trades persistent memory for late-update speed; no workload evidence justifies that change here. |
| 15.5 Assistant mirror stores a second payload | Source observation confirmed. The adjacency-based mirror contract is correct. A locator redesign needs lifecycle/performance justification; it is not necessary for either verified history defect. |

The reports' broader suggestions about prompt shortening, stale-result receipts,
benchmarks against official builds, extra recovery calls, and total model tokens
remain hypotheses. Client history and transport payloads are not automatically
model context. Ordering, successful-response-tail gates, authoritative history,
permission intersection, and mutation/replay safety were retained.

## Source and regression coverage

- MCP conversion, wire projection, and attachment allocation:
  `codex-rs/protocol/src/models.rs`; affected consumer expectations in
  `codex-rs/core/src/session/tests.rs`.
- Call-local versus required denial:
  `codex-rs/tools/src/function_call_error.rs`, `core/src/tools/parallel.rs`,
  `core/src/tools/router.rs`, and router regression tests.
- Telemetry and existing completion/admission fixes:
  `codex-rs/core/src/session/turn.rs` and its existing turn tests.
- Callback settlement, bounded notifications, and exclusive execution:
  `codex-rs/app-server/src/outgoing_message.rs`, thread start/unload processors,
  `thread_state.rs`, and `bespoke_event_handling.rs`.
- Identity and late-event reconstruction:
  `codex-rs/app-server-protocol/src/protocol/thread_history.rs`.

Regression selection covers serialized MCP evidence/status, heterogeneous
attachment order and ceilings, raw/canonical hook mirrors, ambiguous retained
waits, malformed whole-grant denial, notification and listener deadlines,
settlement during backpressure, dynamic execution ownership/replay, shared
terminal classification, and existing telemetry delivery.

The protocol selection passed **90/90 tests**, including the new MCP and
attachment regressions. These passing tests were not rerun. Its log is
`codex-rs/target/test-runner-logs/folder5-docs11-15-protocol.log`.

The history selection (`test(thread_history::)`) completed successfully with
exit status 0, recorded in
`codex-rs/target/test-runner-logs/folder5-docs11-15-history-resume.exit`.
It was not rerun. The resumed process did not retain its per-test console output,
so no exact history-test count is claimed.

The core selection passed **11/11 tests**, plus its Windows stack setup check.
The JUnit artifact is
`codex-rs/target/test-runner-logs/folder5-docs11-15-core-3abd8c7b-a6dc-4fca-a988-86e4789e21d9.xml`.
Those passing tests were not rerun.

The app-server selection passed **78/78 tests**, plus its Windows stack setup
check, with automatic retries disabled. Its complete result log is
`codex-rs/target/test-runner-logs/folder5-docs11-15-app-captured.log`.
This covers the new permission, notification/listener deadline, settlement,
dynamic-owner, and replay regressions together with surrounding transport and
history-state coverage.

The earlier resumed app-server runner returned a command failure without
retaining the underlying diagnostic or producing a test-result report. Its
cause remains unknown. A subsequent discovery-only invocation succeeded;
the unresolved selection was then run from its built binary with explicit
output capture. No recorded passing test selection was rerun. Original
interrupted runs likewise produced no passing results. The initial cold-lane
attempt ended before tests and produced no diagnostic explaining its termination.

Recorded final results: 90 protocol tests, 11 core tests, 78 app-server tests,
and the successful history selection. No full-workspace test run or Desktop
activation was performed. The shared checkout continued changing during
validation, so these results describe the binaries tested, not a frozen release
of all independent workspace edits.
