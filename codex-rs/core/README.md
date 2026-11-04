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
   [session/mod.rs](src/session/mod.rs) owns shared session state and services.
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
| Tool exposure and discovery | [tools/spec_plan.rs](src/tools/spec_plan.rs), [tools/exposure.rs](src/tools/exposure.rs), [tools/handlers/tool_search.rs](src/tools/handlers/tool_search.rs) |
| Agent lifecycle and coordination | [agent/control.rs](src/agent/control.rs), its [domain modules](src/agent/control/), [multi-agent handlers](src/tools/handlers/multi_agents_v2/) |
| History reconstruction and compaction | [session/rollout_reconstruction.rs](src/session/rollout_reconstruction.rs), [compact.rs](src/compact.rs), [compact_provenance.rs](src/compact_provenance.rs) |
| Live recovery evidence | [session/resume_inventory.rs](src/session/resume_inventory.rs), [unified_exec/process_inventory.rs](src/unified_exec/process_inventory.rs) |
| Approval and sandbox execution | [tools/orchestrator.rs](src/tools/orchestrator.rs), [tools/sandboxing.rs](src/tools/sandboxing.rs), [exec_policy.rs](src/exec_policy.rs) |
| MCP session integration | [mcp.rs](src/mcp.rs), [mcp_tool_call.rs](src/mcp_tool_call.rs), [session/mcp_runtime.rs](src/session/mcp_runtime.rs) |

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

The original parent paths keep explicit compatibility re-exports. Follow these
modules for the implementation rather than expanding the orchestration files.

## Follow facades to their owners

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
