# Verification of the five supplied reports

Scope: the five files named `New Text Document.txt` through `New Text Document (5).txt` in the supplied folder. Each file was read completely. Their proposed actions were treated as claims to investigate, not as instructions. No other agents were contacted.

This checkout already contained extensive independent modifications. Findings below refer to the combined local source, not an upstream release. No upstream synchronization, binary replacement, or Desktop restart was performed.

## Findings and disposition

| Report | Claim | Source verification and disposition |
| --- | --- | --- |
| 1.1 | Optional request measurements delay provider completion | Verified in `core/src/client.rs`, `map_response_events`. Completion and the provider baseline now publish before optional diagnostics finish. Required inference-trace recording remains ordered. Diagnostic work is bounded to 32 outstanding attempts, with cancellation and an explicit omission warning on saturation. Completion timing freezes when the provider event arrives. |
| 1.1, related | Late measurements can update the wrong request | Verified: timing setters previously selected the latest row. They now use sampling and physical attempt IDs. Older measurement work cannot replace the latest prompt-context baseline. |
| 1.2 | Native tool-search failures lose their explanation | Verified in `tools/parallel.rs`, `failure_response_for_message`. The incomplete discovery result remains schema-free; a bounded, correlated failure explanation is committed beside it through the ordered response path in `session/turn.rs`. Internal fatal details are sanitized. |
| 1.3 | Receipt substitution can discard usable provider inheritance | Verified in `client.rs`, `prepare_websocket_request`. Canonical fallback now reuses the existing exact-prefix verification before invalidating provider history. A mismatch retains the safe full-request fallback. Receipt consumption uses the effective dispatched input. |
| 1.4 | Baseline capture holds one lock across unrelated workspaces | Already fixed in this checkout: `tools/parallel.rs`, `capture_baseline`, uses a short map lock and separate asynchronous capture slots per key. No second mechanism was added. |
| 1.5 | Retained results should become compact receipts | Conditional optimization. Replay currently returns evidence; safely removing it requires proving that its original body remains in the effective provider context after compaction and rebasing. The report itself conditions this proposal on that proof. No verified redundant replay was established, so evidence was preserved. |
| 2.1 | Late legacy wait completion can attach to a newer turn | Verified in `app-server-protocol/src/protocol/thread_history.rs`, `handle_collab_waiting_end`. It now locates a unique retained owner and checks that owner's terminal status. Ambiguous retained owners are left untouched; unmatched unscoped legacy data keeps the compatibility fallback. |
| 2.2 | Live lifecycle mapping ignores explicit turn ownership | Verified in `event_mapping.rs`, `item_event_to_server_notification`. Nonempty embedded ownership now governs item and command start/completion notifications; empty legacy ownership falls back to the caller. |
| 2, other | Raw ID/path serialization scopes imply duplicate runtime owners | The raw identity discrepancy is real; a duplicate-owner runtime failure is not established by it. Changing the protocol scope alone would not canonicalize aliases and could conflict with running-thread semantics. No speculative scheduler rewrite. Client history size also does not establish model-input size. |
| 3.1 | Ordinary web-service errors are classified as fatal | Verified in `ext/web-search/src/tool.rs`. Setup and service errors now become sanitized model-visible errors with distinct guidance for malformed requests, authentication, quota, policy, temporary failures, and invalid responses. No automatic retries were added. |
| 3.2 | User text bypasses the search-context bound | Verified in `history.rs`, `recent_input`. The complete serialized message projection is now bounded to 16,000 bytes, including metadata, JSON escaping, and omission notices. Commands remain separate. Text is clipped on UTF-8 boundaries before constructing output. |
| 3.3 | Earlier assistant chatter crowds out the latest answer | Verified. Selection now proceeds newest-first under the 4,000-byte assistant allowance, then restores chronological order. |
| 3.4 | Empty and contradictory web commands reach provider setup | Verified. Validation runs before provider/auth/client resolution. It rejects no-operation calls, excessive search batches, missing response-length requirements, and blank page references. Opaque references and the documented empty-query escape hatch remain valid. |
| 3.5 | The web tool description should be shortened | The description is long, but duplicated policy at every call site and safe deletion were not established. Removing user-visible invocation or citation rules would be a behavioral change without verified benefit. Left unchanged. |
| 4.1 | Estimation defaults delegate to contribution methods | Verified API behavior in `ext/extension-api/src/contributors.rs`; a current faulty contributor was not established. Skills already override estimation, and the memories contributor reads prompt data without consuming extension work. Making every implementation migrate to a new trait contract is not justified by a demonstrated failure. |
| 4.2 | Tool contributors default to revision zero | Verified API default. The goal contributor supplies a changing revision; other installed surfaces are stable or configuration-dependent. Routers are reused within a turn, while configuration-dependent surfaces are constructed from that turn's configuration. No stale installed surface was established, so no blanket breaking trait change was made. |
| 5.1 | Partial orchestrator discovery becomes a permanently incomplete catalog | Verified in `ext/skills/src/provider/orchestrator.rs` and `state.rs`. Discovery now retains page cursor, within-page offset, and cursor-loop detection. Explicit `skills.list` continuation advances cached discovery or retries failed pages, keeping each operation bounded to 10 pages, 100 skill resources, and its existing deadline. Prefix-based list cursors remain valid when later pages append entries. |
| 5.2 | Skill load failures lose actionable meaning and look like successful injection | Verified. Typed provider failures produce sanitized recovery guidance. Failed selected loads produce model-visible unavailable-instruction fragments. Same-name host suppression and failed/budget-limited selected host loads are recorded separately from successful instruction injection; the legacy path cannot bypass those outcomes. Invalid responses are rejected, and oversized provider resources explicitly explain that output pagination cannot repair the provider limit. |
| 5.3 | Independent raw 8,000-byte cuts miss aggregate/rendered costs and repeat prefixes | Verified in `render.rs` and `extension.rs`. Selected instructions share a 32,000-byte rendered budget, including escaping, wrappers, and recovery guidance. Small instructions can load completely. Orchestrator continuation begins after the supplied prefix using the existing content-bound cursor. A reserved failure notice covers identities too large to fit. |
| 5.4 | Saturated resource caches never admit the new working set and duplicate reads | Verified in `state.rs`. The existing 100-entry/8 MiB cache now evicts least-recently-used entries, shares immutable results through `Arc`, and coalesces simultaneous reads. Failed reads remain retryable; canceled initialization does not poison the slot. |
| 5.5 | Cold estimation repeats discovery before contribution | Verified repeated observation, but cold estimation deliberately must not populate runtime state, as existing tests require. A new observation-cache lifecycle was not introduced without evidence justifying it. Warm estimation uses the extended catalog, preserving compatible work already present in the shared checkout. |

## Validation

Regression checks cover delayed diagnostics, request attribution, exact-prefix inheritance, discovery failure explanations, retained wait ownership, explicit notification ownership, web error recovery and byte bounds, interrupted discovery, list/read continuation, cache eviction, concurrent reads, and selected-instruction recovery.

Completed checks:

- Nightly Rust formatting completed for the edited Rust files.
- The focused diff whitespace check passed. Git emitted only its configured LF-to-CRLF conversion notices.
- The multi-package test-target compile reached the affected crates, then reported one existing Windows builder mutability error in `core/tests/round_trip_batching.rs` and two ignored setup results in `core/src/tools/handlers/extension_tools.rs`. The builder binding and assertions were repaired. The targeted compile recheck passed without warnings (`validation-reports-1-5-check-repair.log`).
- An earlier compile attempt failed when the shared sccache server disconnected. Validation continued with that cache disabled and an isolated target directory. Interrupted/waiting builds were not treated as passes.

The first focused Nextest run completed 67 tests: 65 passed and two failed (`validation-reports-1-5-tests.log`). The 65 passing results are retained in `validation-reports-1-5-passed-tests.log` and are excluded from subsequent runs. Nextest's local profile disables automatic retries.

The two failures were diagnosed together:

- The canonical-history regression found an incorrect debug assertion that identified restored original output as a substituted receipt. A compatible correction appeared in the shared checkout during diagnosis; it checks the substituted-output hash and was preserved.
- The cache-identity test relied on provider completion waiting for optional diagnostics. It now explicitly waits for the diagnostic task to release its session identity before checking two fully measured requests, retaining its exact eligibility and redaction assertions. The separate completion-withheld-measurement regression already passed and is not rerun.

The failed-only follow-up build ended without a compiler diagnostic before executing tests. It was resumed with the same two-test filter. Both tests passed (`validation-reports-1-5-tests-failed-only-resumed.log`, Nextest run `7eed9eba-d289-4215-8859-db47d218483a`).

**Final result: 67 distinct selected regression tests passed. No passing test was rerun.** The follow-up compiled successfully without warnings. The original run skipped 4,431 tests outside the selected scope; the two-test follow-up skipped 4,136 other core tests.

These are recorded results for the selected regressions at their respective run revisions. The shared checkout received compatible independent edits during validation. Existing passing results were preserved as requested. No full-workspace test result or Desktop activation is claimed.

No measured latency or token-savings claim is made; the reports' projected performance benefits require workload measurements.
