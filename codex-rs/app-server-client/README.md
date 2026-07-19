# codex-app-server-client

Shared app-server client used by conversational CLI surfaces:

- `codex-exec`
- `codex-tui`

## Purpose

This crate centralizes startup and lifecycle management for embedded and remote
`codex-app-server` connections, so CLI clients do not need to duplicate:

- app-server bootstrap and initialize handshake
- in-memory and WebSocket request/event transport wiring
- lifecycle orchestration around caller-provided startup identity
- graceful shutdown behavior

## Startup identity

Callers pass both the app-server `SessionSource` and the initialize
`client_info.name` explicitly when starting the facade.

That keeps thread metadata (for example in `thread/list` and `thread/read`)
aligned with the originating runtime without baking TUI/exec-specific policy
into the shared client layer.

## Transport model

The in-process path uses typed, bounded channels:

- client -> server: `ClientRequest` / `ClientNotification`
- server -> client: `InProcessServerEvent`
  - `ServerRequest`
  - `ServerNotification`

The remote path carries JSON-RPC WebSocket frames over TCP or a Unix socket and
maps them to the same public `AppServerEvent` surface used by callers.

JSON serialization is used at the remote WebSocket boundary, while the
in-process hot path remains typed.

Typed requests still receive app-server responses through the JSON-RPC
result envelope internally. That is intentional: the in-process path is
meant to preserve app-server semantics while removing the process
boundary, not to introduce a second response contract.

## Bootstrap behavior

The client facade returns an initialized connection. Thread bootstrap then
follows normal app-server flow:

- caller sends `thread/start` or `thread/resume`
- app-server returns the immediate typed response
- subsequent thread, turn, and item changes arrive as `ServerNotification`
  events

## Backpressure and shutdown

- Command and event queues are bounded. The in-process path uses
  `DEFAULT_IN_PROCESS_CHANNEL_CAPACITY` by default; remote callers provide the
  shared command/event capacity when connecting.
- Transcript deltas, authoritative item completions, and terminal events wait
  for capacity so visible assistant output remains intact.
- Explicitly best-effort events may be dropped under saturation and are
  summarized by `Lagged`; server requests that cannot be delivered are rejected
  with an overload error.
- `shutdown()` performs a bounded graceful shutdown and then aborts if timeout
  is exceeded.
