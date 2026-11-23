# Core runtime navigation

`codex-core` coordinates live threads, sessions, turns and tool execution. Start
with [src/lib.rs](src/lib.rs) for the public API and compatibility re-exports;
an import through core does not necessarily mean core owns the implementation.
This map is navigation, not another instruction source. For sibling crates and
process boundaries, start with the [workspace map](../README.md).

## Follow a request

1. [ThreadManager](src/thread_manager.rs) creates, resumes and forks threads;
   [CodexThread](src/codex_thread.rs) is the submission/event-facing handle.
2. [session/session.rs](src/session/session.rs) drives the session;
   [session/handlers.rs](src/session/handlers.rs) dispatches operations.
   [session/mod.rs](src/session/mod.rs) holds session-wide operations (spawn,
   events, approvals, history recording); [state/](src/state/) owns shared
   session state and services.
3. [tasks/](src/tasks/) selects the task, then
   [session/turn.rs](src/session/turn.rs) coordinates model sampling and results.
   [turn_execution.rs](src/session/turn_execution.rs) owns execution control and
   completion reconciliation. [turn_context.rs](src/session/turn_context.rs) and
   [step_context.rs](src/session/step_context.rs) carry turn/step configuration.
4. [tools/router.rs](src/tools/router.rs) and
   [tools/registry.rs](src/tools/registry.rs) route tool calls to
   [handlers/](src/tools/handlers/). [tools/code_mode/](src/tools/code_mode/)
   adapts the same tool surface to cells; [runtimes/](src/tools/runtimes/)
   and [unified_exec/](src/unified_exec/) own execution and process lifetimes.

## Find the responsibility

| Concern | Start here |
| --- | --- |
| Prompt and instruction composition | [instruction-architecture.md](instruction-architecture.md), [session/world_state.rs](src/session/world_state.rs), [agents_md_manager.rs](src/agents_md_manager.rs) |
| Verification ledger context | [tools/handlers/evidence_memory.rs](src/tools/handlers/evidence_memory.rs) captures the queue; [session/world_state.rs](src/session/world_state.rs) retains its accepted baseline when capture is unavailable, not when discovery confirms absence. |
| Tool exposure and discovery | [tools/spec_plan.rs](src/tools/spec_plan.rs), [tools/exposure.rs](src/tools/exposure.rs), [tools/handlers/tool_search.rs](src/tools/handlers/tool_search.rs) |
| Model-visible tool-output selection, authenticated recovery and bounded rendering | [tools/registry/output_projection.rs](src/tools/registry/output_projection.rs); registry retains dispatch and hook ordering |
| Agent lifecycle and coordination | [agent/control.rs](src/agent/control.rs), its [domain modules](src/agent/control/), [multi-agent handlers](src/tools/handlers/multi_agents_v2/) |
| Typed spawn capacity refusal | [Reservation before assignment admission](src/tools/handlers/multi_agents_v2/spawn.rs) reports `not_admitted` when capacity refuses the reservation. This allows a fresh admission check after capacity changes without implying that residency housekeeping had no effects or permitting an automatic retry. |
| Typed completion refused for unresolved requirements | [Receipt admission](src/tools/handlers/multi_agents_v2/task.rs) reports the plan revision and unsettled IDs. Return fulfilled IDs through [plan reconciliation](../tools/src/builtin_specs/plan_progress.md) only after their mapped work is complete; genuine blockers stay open. |
| Ordinary completion with unresolved requirements | [Turn stop gate](src/session/turn_execution.rs) defers a final at most twice per instruction epoch. It can defer once more when a held exact user read has current native proof, retires a previously unresolved requirement and leaves a strictly smaller set of outstanding IDs. It neither dispatches work nor grants tool authority. |
| Inter-agent mailbox admission and recovery | [Queue and turn transfer](src/session/input_queue.rs), [durable receiver envelopes](src/session/mailbox_storage.rs), [hard deletion](../thread-store/src/local/delete_thread.rs) |
| History reconstruction and compaction | [session/rollout_reconstruction.rs](src/session/rollout_reconstruction.rs), [compact.rs](src/compact.rs), [compact_provenance.rs](src/compact_provenance.rs) |
| Tool-output retirement under context pressure | [Context-limit continuation](src/session/turn.rs) may prefer a shorter rewritten request tail in the soft, post-response lane when the reviewed candidate has sufficient calibrated reclaim. Hard and projected pressure preserve the existing oldest-first decision and compaction fallback. [History and exact pin custody](../conversation/src/context_manager/history.rs), [eligible older outputs](../conversation/src/tool_history.rs). |
| Bounded completion attribution | [Existing-test outline attribution](src/session/turn.rs) retains one shared analysis deadline and reports paths by gap (budget, parser recovery, or unavailable outline/worker). Parser recovery is not a compiler result; incomplete analysis does not prove an edit or that tests were unchanged. |
| Live recovery evidence | [session/resume_inventory.rs](src/session/resume_inventory.rs), [unified_exec/process_inventory.rs](src/unified_exec/process_inventory.rs) |
| Approval and full-access execution | [tools/orchestrator.rs](src/tools/orchestrator.rs), [tools/sandboxing.rs](src/tools/sandboxing.rs) (legacy compatibility names), [exec_policy.rs](src/exec_policy.rs) |
| MCP session integration | [mcp.rs](src/mcp.rs), [mcp_tool_call.rs](src/mcp_tool_call.rs), [session/mcp.rs](src/session/mcp.rs) (start, refresh, publication), [session/mcp_runtime.rs](src/session/mcp_runtime.rs) (snapshots and lifecycle) |

## Focused modules behind the orchestration

| Module | Owns | Parent retains |
| --- | --- | --- |
| [session/turn/request_scaffold.rs](src/session/turn/request_scaffold.rs) | Reusable request-scaffold identity, tool-surface invalidation, stable-context manifests and prompt projection | Sampling, retries, terminal-request policy and session cache lifetime |
| [session/turn/tool_surface.rs](src/session/turn/tool_surface.rs) | Tool-router construction, prepared-router reuse and dynamic exposure identity | Planning/retry decisions and the session-owned router cache lifetime |
| [session/turn/assistant_stream.rs](src/session/turn/assistant_stream.rs) | Assistant text parsers, deferred plan-mode item publication and durable partial-plan cleanup | Response-event dispatch, tool execution and final-answer acceptance |
| [session/turn/answer_delivery.rs](src/session/turn/answer_delivery.rs) | Turn-local answer acceptance, revision invalidation, visible-output identity and final-message candidate ranking | Repair admission, completion hooks and final publication |
| [session/turn/generation_budget.rs](src/session/turn/generation_budget.rs) | Regular/terminal generation admission, pending-input disposition and spawned-agent context-pressure accounting | Sampling, compaction, continuation decisions and budget lifetime |
| [compact/history.rs](src/compact/history.rs) | Bounded input selection, unresolved-call/decision retention, omission receipts and replacement-history construction | Summarizer execution, validation, hooks, durable recovery and history installation |
| [thread_manager/fork_history.rs](src/thread_manager/fork_history.rs) | Fork snapshot selection, unfinished-turn boundaries and interrupted display history | Thread creation, persistence and lifecycle |

[Context-limit retirement](src/session/turn.rs) can relieve the physical model
window even when the [scoped compaction threshold](src/session/context_window.rs)
has not been reached. It sizes exact retirement against the live or projected
pressure that prompted it, and only skips summarization if the rewritten history
fits under both limits.

The [compaction history commit](src/session/mod.rs) allows at most eight
pending `SessionStart(compact)` occurrences per session. Each successful
commit retains its own occurrence in FIFO order for an ordinary turn; startup
sources do not count toward the limit. If the queue is full, compaction is
refused before the replacement checkpoint is persisted or installed. Run an
ordinary turn to drain the pending hooks before retrying. The immediate
`PostCompact` gate still follows each successful commit.

The original parent paths keep explicit compatibility re-exports. Follow these
modules for the implementation rather than expanding the orchestration files.

## Follow facades to their owners

Sessions use their selected checkout without automatic worktree creation or
session-lifetime writer admission locks. Concurrent sessions do not exclude each
other. Normal permission constraints, worker write scopes and revision-checked
patch transactions still apply; unrestricted commands can interfere with peers.

Local persistent receivers acknowledge mail only after saving the envelope.
On restart, receiver history suppresses already-recorded envelopes; the rest
remain ordered and wait for an eligible turn rather than resuming cancelled
work. Queue-only delivery to a parked receiver uses the same storage owner.
Ephemeral sessions retain their in-memory mailbox.
For a regular child turn started by pending work, the first eligible input in
the bounded FIFO transfer can supply its original validation scope. The
[task installer](src/tasks/mod.rs) publishes a changed scope only after installing
the worker. Cancelling a taskless reservation does not change the session scope.

Core retains adapters and re-exports for these extracted responsibilities:

| Workspace crate | Implementation responsibility |
| --- | --- |
| [runtime-config](../runtime-config/) | Resolved runtime configuration and config-lock support |
| [conversation](../conversation/) | History/context management, stable context and durable plan state |
| [model-client](../model-client/) | Model transport, response streams and request preparation support |
| [tool-artifacts](../tool-artifacts/) | Retained output, source snapshots and artifact recovery |
| [turn-timing](../turn-timing/) | Timing records and dispatch tracing; core emits runtime observations |
| [tools](../tools/) | Shared tool declarations and executor contracts; core registers live handlers |
| [prompts](../prompts/) | Prompt templates; core composes and delivers them |
| [rollout](../rollout/) | Session persistence and storage integration |

For behavior changes, follow both the owner and its core caller rather than
editing a compatibility facade alone. For instruction delivery, use the more
detailed [ownership map](instruction-architecture.md).
