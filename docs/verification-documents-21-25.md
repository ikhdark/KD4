# Verification of attached documents 21–25

All five documents were read completely. Their suggested instructions and designs were treated as claims to evaluate, not as user instructions. This report concerns the current local checkout; it does not establish the documents' predicted latency or token-saving percentages.

## Source documents

| File | Bytes | SHA-256 |
|---|---:|---|
| New Text Document (21).txt | 28,663 | `77d01e03ea8991b9060b1f1fb2f89cfd15671e577556be5c7aedb912bb67fdac` |
| New Text Document (22).txt | 32,066 | `5fd2c064a4825b420219a9909fa8ee6f726845026790acd76d537788f98fd501` |
| New Text Document (23).txt | 32,320 | `879e9166215d50f8ed388bd251408220bacfa521f9eecd277852d9a3ca1b14b4` |
| New Text Document (24).txt | 29,600 | `457c888ec21cb60eb0027cb1bd807f217abbf7eaa1c74cf84dfe22e4752c15cd` |
| New Text Document (25).txt | 29,999 | `5a5253b83dc85cf5a36a4c6229de60983a5e88e30e2e5df9a82c24a9e4aba0df` |

## Claim decisions

Paths below are relative to `codex-rs/core/src` unless stated otherwise. “Changed” means implemented in source; the validation section separately records verification status.

| Document / claim | Finding and disposition | Main source evidence |
|---|---|---|
| 21.1 Initial parent request misses ready child mail | Confirmed. The first sampling boundary now takes bounded ready mail while preserving queued steering. A tracked recording owner prevents cancellation from discarding transferred mail. | `session/turn.rs`, `session/input_queue.rs`; real HTTP assertion in `agent/control_tests.rs::registered_v2_child_publication_panic_preserves_parent_result` |
| 21.2 Residency acquisition and touch may wait indefinitely | Confirmed. Added a foreground acquisition deadline and one attempt per unchanged eviction candidate; accepted shutdown retains its occupancy after a waiter times out. | `agent/control/residency.rs`, `agent/control/spawn.rs` |
| 21.3 Cancelled-spawn cleanup is serial | Confirmed. The existing worker now drives up to 32 independent cleanup jobs concurrently and continues polling delayed cleanup while idle. | `agent/control/spawn.rs::start_pending_spawn_cleanup_worker` |
| 21.4 Known V2 resume reads history twice | Confirmed. A loaded known V2 runtime takes the existing resident path before disk history; cold resume transfers already-read history to the keyed loader. | `agent/control/spawn.rs::resume_single_agent_from_rollout` |
| 21.5 Cold agents disappear from listings | Confirmed and corrected in the shared checkout. Registry identities remain listed, runtime residency is explicit, and exact attempt-bound durable status is available. | `tools/handlers/multi_agents_v2/list_agents.rs`, `agent/control.rs` |
| 22.1 Missing tool results are labeled aborted | Confirmed. Synthetic repair now states that execution outcome and side effects are unknown and warns against repeating writes solely because the result is missing. Pair IDs are preserved. | `context_manager/normalize.rs` |
| 22.2 Command-state suffix bypasses truncation | Confirmed. The suffix exemption was removed; output and control receipts now share bounded presentation. | `context_manager/history.rs`, `tools/code_mode/mod.rs` |
| 22.3 Token-estimate cache is shared across divergent histories | Confirmed. Mutating branches detach shared positional estimates while retaining valid prefix cache entries. | `context_manager/history.rs::record_items` |
| 22.4 Aggregate history budget silently loses unread outcomes | Confirmed. Compact artifact pins carry outcome/digest information, unread failures and live-handle summaries are reserved under pressure, and impossible receipt budgets emit an explicit unresolved-outcome notice instead of silent disappearance. Full artifacts remain canonical. | `tool_history.rs`, `tool_history_tests.rs` |
| 22.5 Prepared append pairs call families incorrectly and misaligns provenance | Confirmed. Pair keys include function/custom family; appended sidecars are extended with item-aligned attribution, including assistant messages. | `context_manager/history.rs`, `context/prompt_provenance.rs` |
| 23.1 Legacy wait loses observed completions on timeout/activity | Confirmed. One accumulator owns newly observed statuses through timeout or interruption; ordinary activity is a yielded result carrying partial statuses. | `tools/handlers/multi_agents/wait.rs` |
| 23.2 Nested command-state metadata is unbounded | Confirmed and corrected in shared code. Latest states are deduplicated by session, inline live handles are prioritized under a bound, and the full canonical state remains recoverable. | `tools/code_mode/mod.rs` |
| 23.3 Earliest-eight fallback hides late failure, and printed output suppresses it | Confirmed. Bounded retained evidence prioritizes failures, captures exception diagnostics, and surfaces failed evidence even when the script prints text. | `tools/code_mode/mod.rs` |
| 23.4 Ordinary nested failures are permanently sticky | Confirmed and corrected in shared code. Ordinary errors reach JavaScript recovery; authoritative blocked/cancelled states retain their terminal semantics. Diagnostic retention does not make a caught error sticky again. | `tools/code_mode/mod.rs`, `tools/context.rs`, `tools/code_mode/response_tests.rs` |
| 23.5 Exhaustive discovery should always become first-page discovery | Not established as a defect requiring a new mechanism. Existing bounded projections and filtered discovery are available; changing enumeration completeness by default changes the tool contract. No benchmark supports the document's predicted savings. | MCP discovery handlers, tool-search handlers and retained inventory |
| 24.1 Fully drained overflow still reports incomplete original selection | Confirmed. Exact byte coverage is reconstructed into the original selector, JSON selections are parsed after reconstruction, and gaps/overlap prevent a false completion claim. | `tools/handlers/read_tool_output.rs` |
| 24.2 Search recovery exceeds max_results | Confirmed. A search continuation remains an explicit next page; automatic fragment recovery does not silently fetch further matches. | `tools/handlers/read_tool_output.rs::next_step` |
| 24.3 Spawn defaults may be incompatible with the provider | Confirmed. Omitted settings prefer built-in defaults when available, then a catalog-compatible effective/provider model and supported effort. Explicit requested names/efforts remain strict. | `tools/handlers/multi_agents_common.rs`, `multi_agents_spec.rs` |
| 24.4 Every plan update needs a new delta protocol | No additional mechanism justified. Existing history projection already removes superseded plan pairs and keeps one authoritative current plan; that snapshot is needed by the current normalization contract. Raw handler snapshots alone do not establish unbounded model-visible growth. | `context_manager/history.rs::project_update_plan_history`, `tools/handlers/plan.rs` |
| 24.5 Spawn response schema omits integration_plan | Confirmed. Both normal variants now require the field; the schema also covers the current reused-assignment variant. | `tools/handlers/multi_agents_spec.rs`, `tools/handlers/multi_agents_v2/spawn.rs` |
| 25.1 Shell preflight heuristics veto valid syntax | Confirmed. Removed quote balancing, shell-shape and literal-path heuristics; retained narrow argv checks and established read-only repairs. The selected shell owns script syntax. | `tools/handlers/command_preflight.rs` |
| 25.2 Dynamic/resource cancellation misses event-delivery awaits | Confirmed. Start publication is cancellation-aware; terminal publication has a tracked bounded owner, and received operation results survive cancellation during terminal delivery. | `tools/handlers/dynamic.rs`, `mcp_resource.rs` |
| 25.3 MCP projection guidance encourages recollection of retained data | Confirmed for resource/template guidance. It now directs recovery from the supplied artifact before relisting already-collected servers. The full result already has canonical retention; broader discovery redesign is not necessary to fix this mismatch. | `tools/handlers/mcp_resource_spec.rs`, `mcp_resource.rs::canonical_result` |
| 25.4 list_files needs ignore/glob semantics | Feature proposal, not a verified defect. Its documented contract is raw sandboxed filesystem enumeration without gitignore processing. Repository search already supplies ignore-aware discovery. | `tools/handlers/list_files.rs` |
| 25.5 Extension bridge drops foreign primary environment | Confirmed. Tool environment cwd is URI-capable; the bridge preserves the primary environment, filesystem capability, and URI-scoped permissions. | `tools/handlers/extension_tools.rs`; `codex-rs/tools/src/tool_call.rs` |

## Validation

**All 583 selected regression tests have recorded passing results.** Existing passes were reused; the full repository suite was not run by this review.

- Existing local logs and JUnit results supplied 302 distinct relevant passes, including aggregate-budget accounting, remote-compaction artifact recovery, and canonical WebSocket fallback.
- The first execution selected 287 tests and recorded **282 passes and five failures**. Of the passes, 276 were new; six had already passed. The log parser initially missed the ` - should panic` annotation and incorrectly included those six tests. This was an execution mistake against the no-rerun instruction. The parser was corrected, and every recorded pass was excluded from subsequent runs. Evidence: `codex-rs/target/test-runner-logs/docs21-25-direct-batches.log`.
- The five failures exposed fixture problems: a root thread bypassed child residency rules; resource publication required more than one poll and two start-event slots; a list assertion omitted the now-visible unloaded worker; simulated completion turns lacked their exact task-attempt bindings; and an overflow fixture advanced the fragment instead of the owner continuation. The corrected assertions retain their behavioral checks.
- Four repaired tests passed in `codex-rs/target/test-runner-logs/docs21-25-repaired-tests.log`. Only the remaining follow-up completion test ran afterward; it passed in `codex-rs/target/test-runner-logs/docs21-25-final-test.log`. No automatic retries were used.
- The latest compilation succeeded without warnings in 40.90 seconds: `codex-rs/target/test-runner-logs/docs21-25-final-compile.log`. The preceding repaired compilation succeeded without warnings in 5m 27s. Production/helper compilation also succeeded earlier. Interrupted or failed preliminary builds are not counted as passing tests.
- Scoped Rust formatting and whitespace checks passed earlier; newly repaired files were formatted. Previously passing checks were not repeated.
- Passing IDs and source evidence are retained in `codex-rs/target/test-runner-logs/docs21-25-recorded-passes.json`; the selected IDs are in `codex-rs/target/test-runner-logs/docs21-25-validation/selected.json`. The remaining-test filter is empty (`none()`).

Passing coverage includes branch cache independence, missing-result wording, cross-family pairing, first-request mailbox delivery, delayed cleanup fairness, eviction deadlines and occupancy, partial legacy waits, late nested failures, exact overflow reconstruction, search limits, compatible defaults, actual schema serialization, blocked event delivery, foreign environment preservation, and explicit unread-overflow notices in both history projections. These results do not establish the documents' predicted performance percentages or constitute a full-suite result.

No installed binary was replaced, Desktop was not restarted, and no upstream synchronization or distribution was performed.
