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
handlers crate. This source refactor does not publish or activate a local binary.

## MCP OAuth completion correlation

Explicit `mcpServer/oauth/login` responses include an optional `loginId`, echoed
by `mcpServer/oauthLogin/completed`. Match attempts by this ID rather than server
name or arrival order; overlapping attempts can finish out of order. Older peers
and automatic plugin logins may omit it. Existing server/thread fields remain.

## Opt-in mobile folder/Git discovery diagnostics

After separately publishing the backend, enable the existing runtime journal in
the backend's launch environment with `CODEX_RUNTIME_OBSERVATIONS=1` and
`CODEX_RUNTIME_OBSERVATIONS_DIR` pointing to an existing absolute private capture
directory (normally under `KDR/captures/`). Publishing alone does not enable it.
These launch-time settings require a new backend process; do not interrupt a
running app or chat without authorization. No phone-app replacement is needed.

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
acknowledgment. This instrumentation diagnoses discovery; it does not itself fix
the mobile Git failure or change filesystem responses, permissions or limits.
