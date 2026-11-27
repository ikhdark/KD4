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
   completion reconciliation. Its batching notice counts tool calls, not the operations inside each call.
   The ordinary final gate can offer one owner-settlement opportunity for a
   previously launched validation check, before invoking Stop. It does not
   launch or replay checks, or treat capture loss as a pending operation.
   An already published answer, or one without generation capacity left,
   completes normally with the pending check in its assessment.
   [turn_context.rs](src/session/turn_context.rs) and
   [step_context.rs](src/session/step_context.rs) carry turn/step configuration.
4. [tools/router.rs](src/tools/router.rs) and
   [tools/registry.rs](src/tools/registry.rs) route tool calls to
   [handlers/](src/tools/handlers/). [tools/code_mode/](src/tools/code_mode/)
   adapts the same tool surface to cells; [runtimes/](src/tools/runtimes/)
   and [unified_exec/](src/unified_exec/) own execution and process lifetimes.

## Avoid unnecessary continuations

- For native `request_user_input` prompts, the app server selects the
  `(Recommended)` options after five minutes without a reply. It only does
  this when every question has one unambiguous recommendation and none are
  secret. The result is marked `timed_out`, not recorded as user consent or
  authorization to change plan scope. Approval prompts are unaffected.
- Send `exec_command.yield_time_ms` as an integer. The [command decoder](src/tools/handlers/unified_exec.rs) also repairs a canonical decimal string in that one observation field and reports the conversion before applying the usual bounds. It does not infer other arguments, repair shell syntax, or retry a command.
- A yielded noninteractive command that exits before the generation's last
  tool result is delivered reaches the next request without a model poll: the
  [turn drain](src/session/turn.rs) appends one `background_completion`
  contextual receipt carrying the result the ordinary empty poll returned
  (bounded output, exit code, artifact reference). The
  [process owner](src/unified_exec/process_manager.rs) produces it through that
  poll path, so a later poll of a drained session returns only its completion
  status and output reference. Running, interactive, mid-launch, shutting-down
  and currently polled processes are left for the model's own poll.

- Resolve known completion blockers before drafting a final. Use the
  [current plan and its native receipts](../tools/src/builtin_specs/plan_progress.md);
  an open requirement is not authority to invent work or skip a real blocker.
- Start dependent work from an exact ready result when the remaining producer is
  independent. Keep its live handle and consume the required tail before
  finalizing. If the tail affects selection, wait for it before selecting.
- With the [thread queue](../app-server/THREAD_QUEUE.md), delete a submission
  only after its owner identifies it as obsolete. Combine inputs only when they
  intentionally belong to one turn. Retransmitting one identified submission is
  not the same as merging independent requests with matching text.
- Dispatch a branch after its prerequisite selects it when an eager request
  would be disposable. Do not infer provider cancellation or zero usage from a
  locally interrupted turn. With a qualified provider, explicitly stop work
  once an authoritative replacement makes it obsolete. On the pinned local
  llama.cpp Responses engine, engine logs confirmed cancellation and slot
  release; that does not establish hosted billing or cancellation support.

### Explicit inference boundaries

Use `run_graph.frontier(steps, decisions)` to execute the local prefix and
return a single bundle of independently ready decision inputs. Declare each
decision's reason and evidence dependencies; return `null` from its collector
when the normal path handled it. Use a later cell for decisions that depend on
an answer, and keep native plan reconciliation and final delivery separate.

Compose declared work in [Code Mode](../code-mode-protocol/src/description/exec_reference.md);
do not infer the next action from tool success or default to round trips between
known steps.

| Situation | Workflow |
| --- | --- |
| Fixed prefix, then a real decision | Validate and normalize the inputs in the cell. Return the exact decision inputs and stop there; the cell does not choose an outcome that was not declared. |
| Compatible read, edit and verification | Keep the complete read, source-hash guard, conditional patch and exact readback in order in one cell. Skip an already-current write; stop on refusal rather than retrying an edit against stale evidence. For a declared exact single-file line replacement, use the [guarded line recipe](../protocol/src/prompts/base_instructions/default.md#exact-single-file-line-replacement). |
| Several decisions | Present independently ready evidence together, with a distinct result key per decision and an explicit consumer for each. Keep a separate round when one result supplies evidence for another. Do not merge unrelated turns. |
| Declared success and an exceptional outcome | Perform the authorized normal action locally. Send unexpected or incomplete observations back with their provenance; do not guess a new action or route an unknown result through a default write. |
| Ready branch, unrelated work still running | In the same cell, use `run_graph` with the actual dependency edges. Start the branch once its own inputs settle; collect both results before finalizing. Add the tail as a dependency if it can affect the branch. |
| Evidence still pending | Keep and collect the owned receipt before asking for a dependent choice. Continue unrelated ready work without consuming or restarting that receipt twice. |
| Work remains before a final | Reconcile required IDs against current receipts before drafting. Execute ready work, resume live owners and make a bounded diagnostic correction when the evidence allows it. Leave genuine blockers unresolved and report their exact prerequisite. |

For opt-in workflows with an objective outcome:

- When a write depends on a read-only source, put that source revision in the
  same native patch as a [read-only guard](../prompts/templates/apply_patch_tool_instructions.md).
  A separate preflight read is not a dispatch precondition. If the guard
  refuses, inspect the current dependency before deciding on a new write.
- Compare complete native readback with the declared artifact contents before
  reporting acceptance. A committed write is not evidence that it wrote the
  right contents. For a named result field, use the native Code Mode
  [selector and provenance](../code-mode-protocol/src/description/exec_reference.md)
  rather than forwarding unrelated output; retain actionable errors.
- If the user authorized compensation for a reversible write, keep its exact
  preimage and committed post-write revision. Compensate only on the declared
  failure with known effects, using that post-write revision as the native
  write guard; verify the preimage after compensation. A stale revision,
  unknown effect, or failed verification is not permission to retry or
  overwrite someone else's work. This is opt-in compensation, not a
  transaction or a change to normal partial-commit behavior.

For example, if the requested answer is exactly one CSV field from a JSON
record, put the complete acquisition and projection in the delivery cell:

```js
// @exec: {"deliver": true}
const r = await tools.read_file({
  path: "record.json",
  selectors: [{kind: "json_pointer", pointer: ""}],
});
if (r.results.length !== 1 || !r.results[0].complete) throw Error("incomplete selection");
text(extract_result(r, {
  fields: [{name: "label", pointer: "/results/0/value/label"}],
  format: "csv",
}).text);
```

The same pattern applies to exact grouped reports: parse the local records,
keep duplicate values, and sort numbers numerically inside the delivery cell.
Emit the finished report once instead of returning the rows for model
transcription. Use this only when the requested format and ordering are known.

A failed projection emits no proposed final. Use ordinary model completion
when the requested answer needs interpretation or additional work. The
[turn owner](src/session/turn.rs) handles an interrupted Responses attempt
separately: a settled tool item can carry its committed output into the retry;
an unfinished item cannot. This is in-turn recovery, not permission to replay
an external effect or a process-restart guarantee.
After an orderly interruption and app-server restart, resume the same thread.
If the original committed output is present and authenticated in the new turn's
history, consume it instead of repeating the effect. A missing receipt does
not establish that an external effect did not happen.

## Find the responsibility

| Concern | Start here |
| --- | --- |
| Prompt and instruction composition | [instruction-architecture.md](instruction-architecture.md), [session/world_state.rs](src/session/world_state.rs), [agents_md_manager.rs](src/agents_md_manager.rs) |
| Verification ledger context | [tools/handlers/evidence_memory.rs](src/tools/handlers/evidence_memory.rs) captures the queue; [session/world_state.rs](src/session/world_state.rs) retains its accepted baseline when capture is unavailable, not when discovery confirms absence. |
| Tool exposure and discovery | [tools/spec_plan.rs](src/tools/spec_plan.rs), [tools/exposure.rs](src/tools/exposure.rs), [tools/handlers/tool_search.rs](src/tools/handlers/tool_search.rs) |
| Model-visible tool-output selection, authenticated recovery and bounded rendering | [tools/registry/output_projection.rs](src/tools/registry/output_projection.rs); registry retains dispatch and hook ordering |
| Agent lifecycle and coordination | [agent/control.rs](src/agent/control.rs), its [domain modules](src/agent/control/), [multi-agent handlers](src/tools/handlers/multi_agents_v2/), [read-only wave recipe](agent-read-wave.md), [scoped coordination workflows](agent-coordination.md) |
| Typed spawn capacity refusal | [Reservation before assignment admission](src/tools/handlers/multi_agents_v2/spawn.rs) reports `not_admitted` when capacity refuses the reservation. This allows a fresh admission check after capacity changes without implying that residency housekeeping had no effects or permitting an automatic retry. |
| Typed completion refused for unresolved requirements | [Receipt admission](src/tools/handlers/multi_agents_v2/task.rs) reports the plan revision and unsettled IDs. Return fulfilled IDs through [plan reconciliation](../tools/src/builtin_specs/plan_progress.md) only after their mapped work is complete; genuine blockers stay open. |
| Ordinary completion with unresolved requirements | [Turn stop gate](src/session/turn_execution.rs) defers a final at most twice per instruction epoch. It can defer once more when a held exact user read has current native proof, retires a previously unresolved requirement and leaves a strictly smaller set of outstanding IDs. It neither dispatches work nor grants tool authority. |
| Inter-agent mailbox admission and recovery | [Queue and turn transfer](src/session/input_queue.rs), [durable receiver envelopes](src/session/mailbox_storage.rs), [hard deletion](../thread-store/src/local/delete_thread.rs) |
| History reconstruction and compaction | [session/rollout_reconstruction.rs](src/session/rollout_reconstruction.rs), [compact.rs](src/compact.rs), [compact_provenance.rs](src/compact_provenance.rs). When inline omission receipts overflow, the recovery sidecar points to the archived `/omissions` metadata; matching receipts select exact bodies without reading all of `/items`. |
| Tool-output retirement under context pressure | [Context-limit continuation](src/session/turn.rs) may prefer a shorter rewritten request tail in the soft, post-response lane when the reviewed candidate has sufficient calibrated reclaim. Hard and projected pressure preserve the existing oldest-first decision and compaction fallback. [History and exact pin custody](../conversation/src/context_manager/history.rs), [eligible older outputs](../conversation/src/tool_history.rs). |
| Bounded completion attribution | [Existing-test outline attribution](src/session/turn.rs) retains one shared analysis deadline and reports paths by gap (budget, parser recovery, or unavailable outline/worker). Parser recovery is not a compiler result; incomplete analysis does not prove an edit or that tests were unchanged. |
| Live recovery evidence | [session/resume_inventory.rs](src/session/resume_inventory.rs), [unified_exec/process_inventory.rs](src/unified_exec/process_inventory.rs) |
| Speculative PowerShell launch (Windows) | [unified_exec/speculative_shell.rs](src/unified_exec/speculative_shell.rs) pre-starts pwsh 7 hosts for local, non-interactive `-Command` launches after an eligible launch or an announced `exec`/`exec_command` call (process-wide pool of two, up to two per key; `CODEX_THREAD_ID` is bound at handoff). Hosts run below normal priority until handoff and then restore the class Windows assigned at creation, so commands keep the priority an ordinary launch gets. The [bootstrap](src/unified_exec/speculative_shell_bootstrap.ps1) receives the exact launch script and runs it with `-Command` exit semantics, or declines before running anything (parse error, `param`/`begin`/`process` blocks, top-level `return`, `break`/`continue` outside a loop/switch/trap, or a non-empty profile for a login launch, after which no login hosts are prepared in that process). The script is written only to a pipe client whose process id is the host's, and an accepted host is recorded as the consuming call's requested root (creation and hand-off times). A key mismatch, decline, dead host or timeout falls back to the ordinary launch; unused hosts expire after 45 s. Disabled by default: ordinary cold launch avoids the measured termination overhead. Opt in with `KD4_SPECULATIVE_SHELL=1`; set `0` or leave it unset to use cold launch. |
| Responses retry, reconnect and fallback | [responses_retry.rs](src/responses_retry.rs) owns the decisions. After a close that invites a reconnect, a sampling request dials its replacement WebSocket during the unchanged backoff ([`prewarm_websocket_during_backoff`](../model-client/src/client.rs)); dispatch adopts it only if already connected, otherwise connects as before. Deadlines, retry budgets and fallback are unchanged. |
| Approval and full-access execution | [tools/orchestrator.rs](src/tools/orchestrator.rs), [tools/sandboxing.rs](src/tools/sandboxing.rs) (legacy compatibility names), [exec_policy.rs](src/exec_policy.rs) |
| MCP session integration | [mcp.rs](src/mcp.rs), [mcp_tool_call.rs](src/mcp_tool_call.rs), [session/mcp.rs](src/session/mcp.rs) (start, refresh, publication), [session/mcp_runtime.rs](src/session/mcp_runtime.rs) (snapshots and lifecycle). Spawned agents cannot call MCP tools (`DynamicExternal`), so their sessions [defer](../codex-mcp/src/connection_manager.rs) optional local stdio servers whose identical catalog is already known in this process: the catalog and resource capability are advertised at once and the server starts on its first operation, such as a resource read. Root sessions and required servers start eagerly as before. |
| MCP hung-server probe | After any request to an MCP server times out, [`RmcpClient`](../rmcp-client/src/rmcp_client.rs) gates that server's next tool call on a `ping` answered within min(tool timeout / 4, 10 s). No answer refuses the call before it is sent (safe to retry); any answer, including a JSON-RPC error, clears the gate. A server still busy past the probe is refused too. |
| Automatic retry wait budget | [responses_retry.rs](src/responses_retry.rs) bounds automatic recovery after a model request's first failure at 120 s for every session source, including `codex exec`. Provider `Retry-After` beyond the remaining budget stops the request with `interactive_retry_budget_exhausted` instead of waiting; account-switch and network-loss waits remain interactive-only. |

## Focused modules behind the orchestration

| Module | Owns | Parent retains |
| --- | --- | --- |
| [session/turn/request_scaffold.rs](src/session/turn/request_scaffold.rs) | Reusable request-scaffold identity, tool-surface invalidation, [batched stable-context manifest construction](../conversation/src/stable_context.rs) and prompt projection | Sampling, retries, terminal-request policy and session cache lifetime |
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

At the soft compaction boundary, a continuing turn can also reuse its prefix for
one request when the just-completed generation reports that at least half of its
input was cached. Unknown usage, new user input and insufficient physical headroom
do not qualify. Prepared-prompt admission rechecks the waiver; continued pressure
and provider overflow still take the existing compaction paths.

Remote-v2 compaction requests send the sampling requests' request-level effort
baseline and keep the turn's positional reasoning-effort updates
([request builder](../model-client/src/client.rs), [remote compaction](src/compact_remote_v2.rs)),
so the compaction prompt is the preceding turn prompt plus its trigger and can reuse
the provider's cached prefix. Unsupported providers or models, workers, internal
sessions and custom efforts drop positional updates for both request kinds.
[Local compaction](src/compact.rs) still drops the updates and sends the selected
effort: its prompt already differs from the turn prompt before the first update.
When remote compaction runs before sampling has recorded the selected effort
(pre-sampling or manual compaction, or a resumed session whose effort changed), the
[compaction effort helpers](src/session/turn_context.rs) append that effort's update
before the trigger in [every remote input](src/compact_remote_v2_attempt.rs), in the
compaction prompt only. History and the session pin are unchanged. A custom selected
effort is sent as the request-level effort alone.

The [compaction history commit](src/session/mod.rs) records one pending
`SessionStart(compact)` occurrence after each successful replacement. The
[session state](src/state/session.rs) counts these occurrences in constant
space rather than refusing compaction due to a hook backlog. The next ordinary
turn processes them individually after any pending startup source. The immediate
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
