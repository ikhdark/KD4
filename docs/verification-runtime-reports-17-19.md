# Verification of runtime reports 17–19

The three requested documents were read in full (94,995 bytes, 1,224 lines). Their directives and proposed implementation order were treated as claims and suggestions, not additional instructions. No other agents were contacted. This audit uses the current local checkout and preserves pre-existing changes; it does not compare against an unverified upstream revision.

Source documents (SHA-256):

- `(17)`: `595d122c24a7529e8ace82bf6356e2051e4ff00838800c2a3c36324cf4141d1f`
- `(18)`: `ab144d79faa06b9a8f3acd48aedd9e3b65ac8b6de5f45b9020528c329889631f`
- `(19)`: `c579b3b60a27255d5b1fbb1758a5e71603cb3bde49dc91d06f08aeaa1b6e8c81`

## Claim disposition

| Claim | Verification and disposition |
| --- | --- |
| 17.1 Unexecuted results reacquire workspace admission | Confirmed. Direct dispatch now carries authoritative handler-entry state to outer completion; unstarted calls and suppressed failures skip evidence registration. Replay no longer acquires an execution lease. The current replay ledger already marks every eligible replay class as requiring fresh observation, so the report does not establish actual successful stale replay in this checkout. That restriction remains. |
| 17.2 Direct validation lacks lease-held revision recording | Confirmed. Recording is now independent of the nested evidence-persistence branch and uses the actual executed payload while the lease remains held. |
| 17.3 Finish notification can replace a settled result with an abort | Confirmed and corrected after required result projection and trace commitment. Completion wins before notification, so cancellation during that notification preserves the original successful or failed result. Notification delivery remains awaited: the production goal contributor performs accounting, so the report's assumption that every finish callback is disposable does not hold. No detached notification service or new timeout was introduced. |
| 17.4 Independently constructed runtimes have independent gates | Confirmed in production: the Code Mode delegate constructs another runtime. TurnContext now owns the coordinator shared by direct/nested runtimes and refreshed steps. Review turns get their own domain. Existing per-resource concurrency remains; outer Code Mode execution still delegates leases to children. |
| 17.5 Baseline map mutex spans unrelated workspace I/O | Confirmed. A short map lock selects a per-cwd capture slot; capture and same-key coalescing remain under that slot. Mutation revisions, unavailable identities, and authoritative non-Git results retain existing semantics. |
| 18.1 Plan-mode errors bypass settlement; failed prefix persistence still opens deferred tools | Confirmed. Plan-mode errors become the attempt outcome. Deferred admission closes successfully only after ordered response persistence succeeds; failures retire deferred work and still drain admitted work. |
| 18.2 Accepted-output recovery combines fresh history with old scaffolding | Confirmed. Recovery captures a new step, revalidates the router, resolves the scaffold, updates the manifest and Code Mode worker, and constructs the prompt from authoritative history. No-output transport retries retain their request; the retry counter stays outside recovery preparation. |
| 18.3 Encoded image bytes control pending-input context pressure | Confirmed. Pending user/response/context items now use the existing model-visible estimator; local images are estimated without loading files during admission. URL images also receive an image allowance. Schema/body-prefix accounting remains separate. This remains an approximation, not provider-calibrated token accounting; no new token-counting service was added. |
| 18.4 Terminal-only omission does not prohibit execution | Confirmed. The generation-scoped runtime policy rejects calls at output acceptance and dispatch, before handler work or capability activation. Subsequent ordinary generations retain normal execution. |
| 18.5 Completion loses a hook block when repair is unavailable | Confirmed at the controller boundary and for exhausted repair admission. A blocking flag now survives independently of repair text and yields an error terminal result when repair cannot proceed. The hook parser normally requires nonempty feedback before producing a block; therefore the report overstates how directly malformed external hook output reaches this branch. Existing parser policy was not rewritten. |
| 19.1 Premature EOF is silent; errors permit subsequent completion | Confirmed. EOF emits a mapped stream failure; the first error terminates processing. Only genuine completion publishes reusable response history. WebSocket warmup explicitly requires completion. |
| 19.2 Completion joins request measurements | The dependency was present at initial inspection. Compatible changes appeared in the shared checkout during verification: the current mapper publishes authoritative history and completion, releases the upstream stream, then resolves optional measurements. Preserved that implementation and included its blocked-measurement regression in the selected client tests. Timing enrichment can therefore finish after consumer completion; no latency percentage is claimed. |
| 19.3 Prefix hashing performs additional serialization | Confirmed mechanism. Existing normalized comparison is the fallback, but hash state also supports startup-prefix proof and baseline identity. Replacement and differential proof across representations are an optimization proposal; no measured regression warrants that broader replacement here. |
| 19.4 Receipt overlap restores full history and resets inheritance | Confirmed conservative fallback. Reproof against a different representation could save replay, but requires coherent stable-context selection, attribution, and subsequent baselines. No incorrect provider transcript or measured net cost was established. Retained conservative inheritance checks; no context/token savings claimed. |
| 19.5 Enabled tracing awaits ordered writes | The awaited writes were present at initial inspection. The shared checkout now includes an ordered background writer with bounded capacity and explicit incomplete-trace warnings, preserving logical companion payloads. Preserved this compatible implementation. Its blocked-writer client regression was included in the selected tests; no additional checkpoint format or tracing framework was introduced. |

## Other assertions

Replay paragraphs and stale-evidence envelopes are real representations, but their net model-token cost and recovery benefits were not measured. Existing replay freshness safeguards remain; no artifact round trip or lossy envelope rewrite was added. Repeated planning, fragmented supplemental budgets, plugin-mention telemetry I/O, and finalizer/repair ordering are source-visible mechanisms, not proof that their proposed caching/queueing policies improve completed tasks. The finalizer repeat-safety guard remains intact.

The reports do not establish production failure frequency, unnecessary model reasoning, extra validator calls, latency percentages, provider token savings, or superiority to upstream. No live-provider benchmark, upstream synchronization, publishing, installed-binary replacement, or Desktop restart was performed.

## Source and regression anchors

- Execution admission, cancellation, baseline capture, and validation: [parallel.rs](../codex-rs/core/src/tools/parallel.rs), [dispatch state](../codex-rs/core/src/tools/context.rs), [registry](../codex-rs/core/src/tools/registry.rs), [turn coordinator ownership](../codex-rs/core/src/session/turn_context.rs).
- Attempt settlement, retry preparation, pending admission, and completion: [turn.rs](../codex-rs/core/src/session/turn.rs), [turn tests](../codex-rs/core/src/session/turn_tests.rs), [output acceptance](../codex-rs/core/src/stream_events_utils.rs), [output tests](../codex-rs/core/src/stream_events_utils_tests.rs).
- Shared image accounting: [history.rs](../codex-rs/core/src/context_manager/history.rs), [history tests](../codex-rs/core/src/context_manager/history_tests.rs).
- Stream terminal behavior and retained diagnostic contracts: [client.rs](../codex-rs/core/src/client.rs), [client tests](../codex-rs/core/src/client_tests.rs).
- Qualification of replay and observer claims: [replay eligibility](../codex-rs/core/src/session/turn_execution.rs), [goal lifecycle accounting](../codex-rs/ext/goal/src/extension.rs), [stop-hook parser](../codex-rs/hooks/src/events/stop.rs).

## Validation

- Initial focused run: **163 executed, 158 passed, 5 failed**. All eight report-specific tests and both completed-notification cancellation cases passed. Passing IDs were saved and excluded from subsequent runs.
- The five failures were fully diagnosed before repairs: validation recording unnecessarily locked the tracker for stateless tools; two workspace tests omitted the production dependency cache; the timing test assumed provider completion joined optional diagnostics; and the prompt-hook fixture omitted the nonempty blocking reason required by the parser.
- The stateless-tool regression was corrected by retaining the existing workspace-capability boundary around revision capture. The other four fixtures were repaired without weakening their expected results. **The follow-up executed exactly those five failures: all five passed. Total: 163 distinct passing tests, zero remaining failures in the selected scope.**
- Scoped rustfmt and Git whitespace checks passed before the failure repairs and were not repeated. Git emitted its configured LF-to-CRLF notices. The completed helper build was reused. It emitted one unrelated unused-method warning for `PendingThreadUnloads::wait_until_finished` in app-server.
- Earlier compilation attempts stopped before tests (including a failed-only build interrupted by another shared-source import change): changing shared source exposed cloud-config/protocol API errors, then an unrelated retry fixture mutated an `Arc` incorrectly and lacked its import. Existing compatible corrections were preserved. A temporary import added here became redundant when that fixture adopted a qualified path and was removed.
- Test output was captured directly after the Windows runner lost inherited diagnostics. The capture wrapper's console encoding error occurred after the complete 163-test run; results were recovered from its intact UTF-8 log, without repeating tests.

Execution evidence:

- [Initial 163-test run](../codex-rs/codex-rs/target/lanes/core-tests/test-runner-logs/rust-test-stderr-crhy7fup.log)
- [Five failures retested successfully](../codex-rs/codex-rs/target/lanes/core-tests/test-runner-logs/rust-test-stderr-4nvj9ufo.log)
- [Distinct passing-test ledger](../codex-rs/codex-rs/target/lanes/core-tests/reports-17-19-results.json)

These are focused local regressions, not the full workspace suite or live-provider performance measurements. No passing tests were rerun. Installed Desktop activation is outside this request.
