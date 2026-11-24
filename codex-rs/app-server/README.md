# App-server crate boundaries

`codex-app-server` owns process startup, database initialization, transport loops,
shutdown coordination, and the public in-process API. Its public entrypoints and
transport/authentication exports remain the compatibility facade for callers.

The implementation is split into local workspace libraries:

- [RPC](../app-server-rpc): outgoing messages, connection sessions and RPC gates,
  request serialization, and RPC errors.
- [Configuration](../app-server-config): configuration loading, mutation and
  refresh ownership, plus API conversions and warnings.
- [Execution](../app-server-exec): command/process RPC adapters and their bounded
  session ownership, delegating execution to the existing process engines.
- [Handlers](../app-server-handlers): request dispatch, thread lifecycle/state,
  event bridges, and remaining endpoint services.

Dependencies point from this runtime to handlers, from handlers to execution,
configuration and RPC, and from execution to configuration and RPC. None of these
libraries depends on this runtime crate. Existing protocol and transport crates
continue to own wire types and transport mechanisms.

Runtime crates depend directly on `codex-app-server-protocol-types`; handlers also
use `codex-app-server-protocol-projection` for event mapping and history replay.
`codex-app-server-protocol-schema` owns generation, and `codex-app-server-protocol`
remains the compatibility facade and schema-export CLI package. Runtime-only
consumers do not depend on that tooling facade.

The [thread follow-up queue contract](THREAD_QUEUE.md) is implemented by the
handlers crate.

`thread/rollback` returns the durable post-rollback history. Its turn statuses
use the same stale-turn projection as history reads: only the matching live
turn can remain `inProgress`. Completed and failed turns, items, and ordering
are unchanged by that status projection.

For an explicit request to retrieve an already recorded answer by thread and
turn ID, clients can call `thread/read` with `includeTurns: true` rather than
start a model turn. Check the returned thread and turn IDs, completed status,
and the expected answer item before using it. Missing or ambiguous history
is not a successful lookup; keep the ordinary turn path available for work
that actually requires inference. This does not route free-form chat input.

When the caller has authorized creating an exact command working directory,
prefer `fs/createDirectory` (use `recursive: true` only when authorized to create
its parents) over a shell preflight. Use an absolute path and require successful
creation before launching in that directory. An existing directory is accepted;
an obstructing file is an error, not permission to overwrite it, substitute a
different working directory, or restart an unrelated process.

## Soft reserve for explicitly optional client work

A client that submits its own phases can defer a discretionary `turn/start`
when its soft token target would leave too little room for mandatory phases.
The caller must explicitly classify the discretionary phase as deferrable.
Do not infer that classification from elapsed turns, model text, or a plan label.

Use [thread token usage](../app-server-protocol-types/src/protocol/v2/thread.rs)
from the latest settled turn for the matching thread and turn IDs. Subtract
the thread total at workflow start to obtain `spent`; do not sum cumulative
updates. `remaining_required` and `optional_cost` are client estimates in the
same input-plus-output token units. If any input is unknown, use the client's
ordinary policy rather than treating it as zero.

```python
def should_defer_optional(spent, soft_target, remaining_required, optional_cost):
    values = (spent, soft_target, remaining_required, optional_cost)
    if any(type(value) is not int or value < 0 for value in values):
        return False
    return spent + remaining_required + optional_cost > soft_target
```

Apply this decision only before starting a discretionary turn. Record a
deferred turn as not run, not completed. Continue required work, including
newly discovered work, even if the estimate was wrong or the soft target
has already been exceeded. This does not interrupt live turns, change
server admission or retry behavior, reserve provider quota, or estimate a bill.

## MCP OAuth completion correlation

Explicit `mcpServer/oauth/login` responses include an optional `loginId`, echoed
by `mcpServer/oauthLogin/completed`. Match attempts by this ID rather than server
name or arrival order; overlapping attempts can finish out of order. Older peers
and automatic plugin logins may omit it. Existing server/thread fields remain.

## Opt-in hooks listing diagnostics

In the bounded runtime journal, stdio and remote-control `hooks/list` ingress
and the catalog owner phases share one operation ID. Use the typed request
hash on ingress to match the client call. Other transports keep catalog phases
without claiming an ingress join. This does not record cwd strings, change
hook discovery or imply client consumption.

## Mobile RPC boundaries

Experimental RPC capability is per logical connection. Remote-control
temporary failures use error `-32002` with `data.reason`, `retryable: true`
and an optional RFC 3339 `retryAt`. An authoritative managed denial is not a
temporary service cooldown. `fs/watch` capacity failures use `-32003` with the
limiting scope; unwatch releases capacity. File reads still require an absolute
path in the selected host environment, not a phone-side display label. A
filtered `thread/list` page may be empty with a continuation: keep reading
until `nextCursor` is null.

## Opt-in mobile folder/Git discovery diagnostics

After separately publishing the backend, enable the existing runtime journal in
the backend's launch environment with `CODEX_RUNTIME_OBSERVATIONS=1` and
`CODEX_RUNTIME_OBSERVATIONS_DIR` pointing to an existing absolute private capture
directory (normally under `KDR/captures/`). Publishing alone does not enable it.
These launch-time settings require a new backend process; do not interrupt a
running app or chat without authorization. No phone-app replacement is needed.
The [native admission receipt](../turn-timing/README.md#mobile-backend-admission)
identifies the producer and its resolved startup-policy projection, not the live
Desktop connection or the complete backend configuration.

Reopen the phone's Git panel and switch workspaces. The journal correlates
`mobile_rpc_received`, admission/handler outcomes, `mobile_directory_path_decoded`
and `mobile_file_read_*` records by `operation_id`. Directory observations retain
the incoming/decoded path's SHA-256, byte count and syntax class, filesystem error
kind/OS code, response-budget failures, entry count and whether `.git` was returned
as a file or directory. Hash the exact known workspace-path UTF-8 string to compare
its fingerprint. No raw paths, other entry names, contents, commands, credentials
or error-message bodies are added by these diagnostics.

Only transport-identified remote-control requests receive these observations.
Existing journal caps and failure behavior remain in force. An abandoned span,
overflow, missing events or absent phone observations is incomplete evidence,
not a successful lookup. Handler completion is not a phone-rendering or delivery
acknowledgment. This instrumentation diagnoses discovery; it does not change
filesystem responses, permissions or limits. For the original observations and disposition, see the
[mobile Git-panel case](../../../KDR/audits/closed-cases/mobile-git-panel-20261115/current-findings.txt).
