# Verification of folder (5), documents 16–20

All five files were read completely. Their directives and proposed implementations were treated as audit material, not as user instructions. Work was performed without contacting or spawning other agents. Verification concerns the current local checkout, which already contained extensive independent edits; it does not establish an upstream regression or a measured speedup.

## Input identity

The inputs are `New Text Document (16).txt` through `New Text Document (20).txt` in the user's `Desktop/New folder (5)` directory. Files 19 and 20 are byte-identical, but both were read in full.

| File | SHA-256 |
| --- | --- |
| 16 | `E1E99B062A0E25DD2F02A1BCC5C1A34D0162BE77C4151766462F8167B2EC81F4` |
| 17 | `75386BF6CB36B670117426FAC9FB34888134DDC33AC5620B560AB6423BBBEE46` |
| 18 | `7D509FDB19F28B269E4C58C585BCDD75D7ED2FE352C4651BDFE96D9B810E86F1` |
| 19, 20 | `FBECEB0978A1434166355C2614A0E62EADCA8A9F8594D5F0CC2575BD5D2CFB7B` |

## Claims and disposition

| Claim | Verification and disposition |
| --- | --- |
| 16.1 — retained goal executors can mutate after turn cleanup/disable | Confirmed in `ext/goal/src/tool.rs`: the previous guard accepted `None`. Added lifecycle-owned live-turn admission independent of accounting, cancellation-aware permit acquisition, and enabled/eligible/current-turn checks after acquisition. Stop/abort close admission before waiting for accounting; thread stop disables the runtime. |
| 16.2 — one goal's terminal error suppresses later goals | Confirmed in `ext/goal/src/runtime.rs`. Suppression now records the failed goal ID. A different goal is not suppressed, while explicit external activation can rearm the existing goal. |
| 16.3 — completion can target a replaced/edited objective | Confirmed. `update_goal` now requires `goal_ref`, derived from the goal ID and SHA-256 of its objective. Tool responses and goal steering publish that reference. Validation and the terminal mutation share the existing goal-state permit used by external setters. Stale calls return the current reference/objective without completing it. Initial/external replacement steering is included, with one latest pending update retained for retry. |
| 16.4 — usage persistence failure prevents terminal update | Confirmed. A terminal tool call freezes failed accounting and still attempts the status write. Responses disclose `accountingPending` and omit the final-usage instruction when totals are pending. Retry accounting accepts completed goals without reopening them. A status-write failure returns an error and leaves local suppression in place. |
| 16.5 — duplicated accounting, divergent `Unchanged`, old debt blocks current progress | Confirmed and consolidated into the runtime accountant. Current progress is attempted even when older debt fails. The extra storage read used for transition telemetry remains: removing it correctly requires the mutation to expose its previous status, and the report does not measure its cost. No claim of reduced SQL calls or measured latency is made. |
| 17.1 — setup reports success after listener attachment fails | Confirmed in start, cold resume, and fork. Attachment errors now fail setup and use the existing instance-aware rollback path. Closed requesting connections stop the response path without tearing down a possibly shared instance. Best-effort logging remains for background attachment. |
| 17.2 — dynamic results erase recovery information and valid text | Confirmed in `app-server/src/dynamic_tools.rs`. Client error codes, lost replies, and malformed schemas now have distinct bounded messages. Mixed results retain valid content and add an explicit partial-output notice. The client's execution-success flag remains distinct from output completeness; turn-transition cancellation remains separate. Raw client error data is not copied into model context. |
| 17.3 — resume waits indefinitely for unload | Confirmed. Resume now returns the existing closing error with `reason: threadClosing` while the unload owner retains its marker and responsibility for physical termination. |
| 17.4 — unrelated threads share one admission semaphore | Confirmed. Gates are now keyed by thread, with weak map entries and strong references retained by waiters/holders to prevent split gate ownership. |
| 17.5 — non-disconnectable writer blocks shared routing | Production reachability confirmed in stdio/in-process setup. Shared routers now receive explicit failure tokens for these connections. The remaining awaited writer path is used by the isolated in-process notification owner, where it cannot block another connection or request/reply delivery. |
| 18.1 — reserved control admission still waits for running ordered handlers | Confirmed in `request_serialization.rs`. Control requests use a separate drain, retain connection-gate ownership, and have separate global/per-key capacity. Ordered mutation FIFO is preserved. `turn_interrupt` already validates turn identity in app-server and core. |
| 18.2 — in-process lossless delivery blocks commands and replies | Confirmed. Notifications now have a separate bounded producer channel and one ordered delivery owner; responses and client replies retain their own path. Transcript backpressure reaches notification producers. Failed delivery remains explicit, and writer acknowledgement still follows event delivery. |
| 18.3 — queue limits stop counting at dequeue and omit unscoped requests | Confirmed. Outstanding-work leases now cover initialized requests, including unscoped requests, and remain owned by both the handler and final-response context. Separate global/per-connection operation and byte budgets are released on terminal response or disconnect; controls have reserved count capacity. |
| 18.4 — infrastructure failure is observed only after sequential joins | Confirmed. Runtime owners are now observed concurrently. Failure immediately cancels transport; remaining owners receive a bounded grace period and are then aborted/joined. Healthy processor completion still allows outbound output to drain. |
| 18.5 — guard-drop status tasks can publish obsolete state out of order | Confirmed. Status mutations enqueue under the canonical lock to one bounded publication owner. Guard drops synchronously update local state and enqueue; they no longer spawn publication tasks. Overflow explicitly fails delivery instead of silently losing status ordering. |
| 19/20.1 — optional diagnostics gate provider completion | Partly already repaired: request measurements were already resolved after authoritative history and completion publication. Optional trace writes still blocked dispatch/completion. Trace recording now uses bounded per-attempt ordered workers and a global concurrency cap; saturation/failure is identified as an incomplete trace. Execution-critical history/response-tail barriers remain. |
| 19/20.2 — native tool search discards its failure cause | Already addressed in the current working tree by bounded associated `tool_search_failure` context and completion-recording wiring. The native result remains valid and contains no fabricated tool definition. Preserved this work. |
| 19/20.3 — prefix reuse reserializes inherited history | The hashing cost exists, but a superiority claim for the alternate algorithm is unmeasured. Hash baselines also participate in startup-prewarm and stable-context inheritance proofs. Preserved these checks rather than replacing the decision algorithm without the report's proposed differential/performance evidence. |
| 19/20.4 — one baseline lock serializes different workspaces | Already fixed in the current tree: `WorkspaceEvidenceGenerationBatch::capture_baseline` obtains a per-path capture slot under a short map lock and awaits under that slot. Same-key captures still coalesce; independent workspaces have separate locks. |
| 19/20.5 — optional lifecycle observers can fatally block execution | No installed passive start observer was identified. The production tool-lifecycle contributor is the goal extension, which performs accounting and budget steering on finish. Unclassified contributors can establish required state; the test panic matrix does not establish a passive production failure. No new observer framework or blanket fail-open behavior was added. |

## Other claims and boundaries

The reports' estimates of completion rate, token savings, latency magnitude, and superiority to official Codex are unmeasured. Client history, response metadata, alternative prepared prompt views, and transport payloads are not automatically repeated model input. No prompt shortening, blanket validation removal, new planner, universal retry limit, or automatic replay of uncertain mutations was justified.

Status-only goal continuation, listener command priority, warm-resume history seeding, configuration-cache miss coalescing, MCP-refresh ordering, tool-record verbosity, and receipt/replay efficiency remain measurement hypotheses rather than demonstrated additional failures in these reports. Existing cancellation fences, instance/generation checks, successful-response-tail tool admission, history commits, budget-overshoot accounting, and replay ordering were retained.

## Validation

Validation uses the local Nextest profile with automatic retries disabled. No passing test case was rerun. The first compilation was interrupted before producing the goal test binary; no passing result was inferred from it. Test source was updated where the old expected behavior encoded the verified defect.

| Selection | Result |
| --- | --- |
| Goal backend integration | 29 passed across two runs: 28 initial passes, then only the failed resume-idle fixture was corrected and rerun. |
| Goal accounting | 5 passed. |
| App-server library: serialization, status, dynamic output, in-process delivery, runtime supervision, request leases, and thread lifecycle | 69 passed; 454 unrelated tests excluded. |
| Dynamic-tool remote-image integration | 1 passed, including the actual follow-up model request. |
| Trace completion/cancellation and the repaired retry fixture | 4 passed; 4,134 unrelated tests excluded. |

**Final result: 108 distinct test cases passed.** The goal backend's one initial test failure was corrected and rerun alone. Later retries followed compilation failures, before those selections had executed any tests.

An initial accounting compilation encountered a concurrently edited core syntax error that was subsequently removed in the working tree. The initial app-server attempt ended before tests and did not retain child diagnostics; its fully logged retry passed. Core test compilation exposed an assignment through an `Arc` in `core/src/responses_retry_tests.rs`; the fixture now obtains its uniquely owned context before setting the interactive session source. The obsolete unload-wait helper and notification are retained only in test builds, removing the production dead-code warning. These were validation blockers, not passing tests that were rerun.

The targeted whitespace check passed once. Git's LF-to-CRLF conversion notices reflect repository line-ending configuration. This was focused validation, not a full workspace test run. Performance estimates and superiority claims were not benchmarked.

These changes are source-only. No installed Desktop binary was replaced and Desktop was not restarted.
