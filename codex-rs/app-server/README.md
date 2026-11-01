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
