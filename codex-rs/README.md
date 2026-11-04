# Rust workspace navigation

Start here to locate an implementation owner without expanding the whole
workspace. [Cargo.toml](Cargo.toml) declares crate membership and dependency paths;
each crate's `src/lib.rs` lists its modules and re-exports. This is a source map,
not another instruction source.

## Entry points and request flow

| Surface | Entry point | Follow next |
| --- | --- | --- |
| CLI commands | [cli/src/main.rs](cli/src/main.rs) | Command dispatch selects the app server, [exec](exec/), or another command owner |
| Desktop / JSON-RPC service | [app-server/src/lib.rs](app-server/src/lib.rs) | [app-server-handlers](app-server-handlers/) routes requests into the runtime |
| Live thread, session and turn execution | [Core navigation](core/README.md) | Thread manager → session handlers → tasks → turn loop → tool router |
| Client-facing wire contract | [Protocol navigation](app-server-protocol/README.md) | Types, projections and schema generation have separate owners |

## Find the responsibility

| Concern | Implementation owner / starting point |
| --- | --- |
| Turn orchestration, completion and agent lifecycle | [core](core/README.md) |
| Instruction composition and delivery | [Instruction architecture](core/instruction-architecture.md) |
| Shared tool declarations and executor contracts | [tools](tools/); live handlers and registration are in [core/src/tools](core/src/tools/) |
| Retained tool output, source snapshots and recovery | [tool-artifacts](tool-artifacts/) |
| History, context projection and durable plan state | [conversation](conversation/); live installation remains in core |
| Model requests and response streams | [model-client](model-client/); core supplies live turn state |
| Resolved runtime configuration | [runtime-config](runtime-config/); [config](config/) owns configuration support and the [layer loader](config/src/loader/README.md) |
| Session persistence | [rollout](rollout/README.md) |
| Timing records and dispatch tracing | [turn-timing](turn-timing/README.md); core emits runtime observations |
| Execution service and process transport | [exec-server](exec-server/README.md), [exec-server-protocol](exec-server-protocol/) |
| Extension interfaces and built-ins | [ext](ext/), starting with the [extension API](ext/extension-api/notes.md) |
| Shared utilities | [utils](utils/); use a specific utility crate rather than a core facade |

## Code Mode: client, host, runtime, contract

These similarly named crates serve different sides of the process boundary:

1. [core/src/tools/code_mode](core/src/tools/code_mode/) adapts live tools to cells.
2. [code-mode](code-mode/src/lib.rs) is the process-owned session client; it does
   not link V8.
3. [code-mode-host](code-mode-host/src/lib.rs) owns the standalone host connection.
4. [code-mode-runtime](code-mode-runtime/src/lib.rs) owns V8 execution, cell actors
   and in-process sessions; the host links it in production.
5. [code-mode-protocol](code-mode-protocol/src/lib.rs) owns shared session/host
   contracts, tool descriptions and output projection.

Follow the client and host sides when changing their shared contract; a
re-export in `code-mode` does not make that crate the implementation owner.

## App server: service, handlers, wire models

- [app-server](app-server/) owns service startup and connection orchestration.
- [app-server-handlers](app-server-handlers/src/lib.rs) owns request processors.
- [app-server-transport](app-server-transport/) and
  [app-server-rpc](app-server-rpc/) supply transport and RPC support.
- [app-server-protocol](app-server-protocol/src/lib.rs) is a compatibility facade:
  follow [types](app-server-protocol-types/),
  [projections](app-server-protocol-projection/), or
  [schema generation](app-server-protocol-schema/) for implementation.
  The separate [protocol](protocol/) crate contains core event models.

## Follow a facade before editing

Core retains compatibility re-exports for extracted owners. Read the small
facade, follow the owning crate, then check its live caller; avoid adding a second
implementation to the facade. The [core map](core/README.md) identifies focused
modules behind the turn loop.

Build lanes, toolchain working-directory requirements and frozen bundle reuse
are documented in [repository scripts](../scripts/README.md). Repository policy
remains in [AGENTS.md](../AGENTS.md), not in these navigation maps.
