# Verification of reports 42, 43, and the unnumbered report

All three documents were read completely. Embedded commands, implementation orders,
and proposed designs were treated as claims to evaluate, not additional user instructions.
Verification targets the current working checkout, including concurrent changes.

| Document | Lines | Bytes | SHA-256 |
| --- | ---: | ---: | --- |
| New Text Document (42).txt | 463 | 35,658 | `8A7CD4BB1BEB61C2F5F2D0D25E05C097D5A9F827776305794C2BFDBBC4E2B219` |
| New Text Document (43).txt | 442 | 33,261 | `32F8B53C8E6C7CE58043C9A361B844968B04CB0FA5270CA06B0B0B30ED55134D` |
| New Text Document.txt | 420 | 33,383 | `9F5C3E55EB5B7FDFE45030954E0173534123EBC0D871DE8E0A789ED70281B330` |

## Disposition

| Claim | Verification and action |
| --- | --- |
| 42: Persistence failure can look like accepted input or stale planning | Confirmed. Conversation/input/context publication now returns errors through callers. Stale generation remains a distinct successful `None`. Accepted input events follow successful publication; required response barriers fail tool dispatch and sampling. Context history and its candidate baseline share the existing owned commit. |
| 42: Canceled MCP refresh can repeatedly return the old mismatched runtime | Confirmed. The step boundary returns cancellation when an unavailable refresh cannot satisfy its environment identity. An already matching published runtime remains usable. |
| 42: A timed-out catalog looks like an authoritative empty catalog | Confirmed. Snapshot availability is explicit, and sampling receives a request-scoped warning to avoid claiming capability absence. Local work can continue. |
| 42: Refreshed AGENTS.md can be omitted without DeferredExecutor | Confirmed. Every sampling step publishes instruction changes. The nondeferred path replaces only the instruction section of immutable world state; other sections retain their existing ownership. Publication failure cannot advance the candidate baseline. |
| 42: Spawn permission is granted by incidental or negated discussion | Confirmed. Compatibility parsing accepts anchored direct requests with agent targets, recognizes explicit denials and direct question requests, and rejects quoted/incidental examples. Existing task-scoped enforcement remains the authority. No second permission system was introduced. |
| 42: Blocking prompt hooks run after planning | Confirmed. Initial input inspection precedes planning; only admitted inputs and bounded hook context enter planning and persistence. Planning retries do not rerun the hook. |
| 42: Repeated convergence advice accumulates | Confirmed. Reuses existing developer-context deduplication across distinct convergence decisions. |
| 43: V2 retained unresolved input is removed by the installation filter | Confirmed. Only provider output is filtered as consumed transcript; the locally retained user/agent tail survives installation. Exact omitted text is saved through the existing recovery-artifact path before replacement is allowed. |
| 43: Compaction rebasing claims local context was inherited by the provider | Confirmed. Removed the positional rebase. Locally installed instructions, recovery references, and artifact pins are sent in a complete checkpoint before incremental inheritance resumes. |
| 43: Delegated cancellation can block on the child submission queue | Confirmed. Forwarded operations and approval replies are cancellation-aware. Shutdown closes admission immediately and runs actual interruption in a session-owned task; the bounded event drain cannot cancel that teardown. |
| 43: Completion awaits request diagnostics | Mechanism confirmed. This also ensures final timing/usage snapshots include the completed attempt. The report supplies no measurement establishing a harmful delay. Retained the existing completion contract rather than dropping or racing accounting. |
| 43: Receipt fallback can restore raw history after inheritance fails | Mechanism confirmed. Full fallback is a deliberate correctness path when inheritance is unproven. No matched-workload evidence establishes the proposed efficiency gain or proves safe deletion of the fallback. Retained it. |
| 43: V2 fitting omits final request components | Confirmed and corrected. Fitting now measures the largest of the four assembled request representations, including its synthetic trigger, and adds the actual tool-schema estimate. Rewritten output is then assembled again through the same prompt builder. Estimates remain approximate and do not claim to model provider-specific tokenization exactly. |

The unnumbered report is also covered by the concurrent audit in
[attached-report-verification-2026-09-21.md](attached-report-verification-2026-09-21.md).
Its shared fixes were preserved: cell-local congestion handling, prompt cancellation
release, cancellation-cause retention, delegate serialization outside the global lock,
and bounded executable runtime shutdown. Structured output-loss metadata already
existed, so the claim that loss is communicated only as prose is outdated. A new
connection scheduler, aggregate byte-credit system, and retained-result replay
protocol remain proposals rather than demonstrated required fixes.

## Source evidence

- Publication and atomic context staging: `core/src/session/mod.rs`,
  `record_user_prompt_and_emit_turn_item`, `commit_prepared_context_update`, and
  `record_step_world_state_if_changed`; propagation through
  `core/src/hook_runtime.rs`, `core/src/session/turn.rs`, and
  `core/src/stream_events_utils.rs`.
- MCP cancellation and catalog availability: `core/src/session/mcp.rs`,
  `mcp_runtime_for_step`, and `core/src/session/step_context.rs`,
  `bounded_mcp_snapshot`.
- Permission compatibility grammar: `core/src/session/multi_agents.rs`,
  `parse_spawn_authorization_directive`. Early hook inspection and convergence
  deduplication are in `core/src/session/turn.rs`.
- Compaction: `core/src/compact_remote_v2.rs`, `prepare_v2_retained_input`;
  `core/src/compact_remote.rs`, `process_compacted_history_with_retained_input`;
  `core/src/compact_remote_v2_attempt.rs`, `largest_compaction_request_input`.
  Exact recovery reuses `core/src/compact.rs`, `persist_compaction_text_recovery`.
- Provider inheritance and retained diagnostic/fallback behavior:
  `core/src/client.rs`, `invalidate_provider_history_inheritance`,
  `prepare_websocket_request`, and the `ResponseEvent::Completed` handler.
- Delegate cancellation: `core/src/codex_delegate.rs`, `forward_ops`, approval
  reply handlers, and `shutdown_delegate`.

Paths above are relative to `codex-rs/`. Regression tests exercise both the
failure path and successful behavior, including installed checkpoints rather
than only the intermediate retention builder.

## Validation

Focused compilation and regressions are in progress. Failed compilation runs did
not execute tests. Final results will be recorded here when the run finishes.

Production binaries were not installed or activated, and Desktop was not restarted.
No upstream synchronization or distribution was performed.
