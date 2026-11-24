# Rust workspace navigation

Start here to locate an implementation owner without expanding the whole
workspace. [Cargo.toml](Cargo.toml) declares crate membership and dependency paths;
each crate's `src/lib.rs` lists its modules and re-exports. This is a source map,
not another instruction source.

## Entry points and request flow

| Surface | Entry point | Follow next |
| --- | --- | --- |
| CLI commands | [cli/src/main.rs](cli/src/main.rs) | Command dispatch selects the app server, [exec](exec/README.md), or another command owner |
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
| Model requests and response streams | [model-client](model-client/); core supplies live turn state. [Responses HTTP serialization](codex-api/src/common.rs) keeps unchanged client metadata in stable key order on retries; WebSocket serialization is unchanged. |
| WebSocket destination admission | The [shared connector](websocket-client/src/lib.rs) rejects unsupported URI schemes before proxy resolution and outbound admission. Supported destinations still go through normal route and dispatch policy checks. |
| Windows process launch refusal | The [unified exec command handler](core/src/tools/handlers/unified_exec/exec_command.rs) distinguishes native Windows pipe creation access refusal and missing launch inputs from ordinary exit or other startup failures. This does not infer sandbox policy from subprocess text. |
| Request cache diagnostics (not provider-history admission) | [model-client/src/client/request_measurements.rs](model-client/src/client/request_measurements.rs); transport selection and ordered state lifetime remain in client.rs |
| Completed-tool history disk compatibility and journal replay | [conversation/src/tool_history/persistence.rs](conversation/src/tool_history/persistence.rs); public loader/writer paths remain in tool_history |
| Resolved runtime configuration | [runtime-config](runtime-config/); [config](config/) owns configuration support and the [layer loader](config/src/loader/README.md) |
| Session persistence | [Rollout storage](rollout/README.md); [SQLite initialization and recovery](state/README.md) |
| Timing records and dispatch tracing | [turn-timing](turn-timing/README.md); core emits runtime observations |
| Turn analytics | [analytics reducer](analytics/src/reducer.rs); completed turns awaiting enrichment use the same fallback at queue drain and before bounded FIFO eviction |
| Execution service and process transport | [exec-server](exec-server/README.md), [exec-server-protocol](exec-server-protocol/) |
| Extension interfaces and built-ins | [ext](ext/), starting with the [extension API](ext/extension-api/src/lib.rs) |
| Bundled skills | [Source assets](skills/src/assets/samples/); see [skills architecture](core/instruction-architecture.md#extensions-skills-apps-and-plugin-guidance) for staging, discovery and delivery |
| Shared utilities | [utils](utils/); use a specific utility crate rather than a core facade |

## CLI capability metadata

`codex completion --describe app-server/generate-json-schema` emits JSON
for one visible command, without invoking it. Use `.` for the root. It
reports visible child commands, argument names and positions, value counts,
enumerated choices, repeatability and unconditional requiredness from the
[Clap command](cli/src/main.rs). Conditional requirements and runtime policy
are not an invocation guarantee; the command parser remains authoritative.
Plain `codex completion [SHELL]` still emits a shell script.

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
   See the [input projection recipes](code-mode-protocol/input-projection.md) for sparse refreshes, diagnostic rows, overlapping reads, failure receipts and shared source bindings.

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
