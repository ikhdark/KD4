# App-server protocol

`codex app-server` exposes the process-facing JSON-RPC API. Over stdio, messages
are newline-delimited JSON. Start with an `initialize` request containing
`clientInfo` (`name`, `version`, and optional `title`) and optional `capabilities`.
After receiving the response, send the `initialized` notification. The Python
client's [initialization implementation](../../sdk/python/src/openai_codex/client.py)
is an executable example.

Use `thread/start` or `thread/resume` to obtain a thread, then `turn/start` to
submit input. Request responses acknowledge the operation; asynchronous server
notifications deliver turn progress and item updates. Clients must also handle
server requests, including approval and user-input requests, and respond using
the original request ID. The [method registry](../app-server-protocol/src/protocol/common.rs)
defines method names and their parameter and response types.

## Contracts and SDKs

- [JSON schema bundle](../app-server-protocol/schema/json/codex_app_server_protocol.schemas.json)
  describes the checked-in wire contract.
- [TypeScript exports](../app-server-protocol/schema/typescript/index.ts) describe
  the same app-server contract.
- [Python SDK](../../sdk/python/src/openai_codex/api.py) uses generated app-server
  models. Regenerate them from the repository root with
  `uv run --directory sdk/python --group dev python scripts/update_sdk_artifacts.py generate-types`.
  `just sdk-python-check` includes the regeneration freshness test.
- The [TypeScript SDK](../../sdk/typescript/src/index.ts) runs `codex exec --json`.
  Its [item types](../../sdk/typescript/src/items.ts) describe exec's smaller,
  snake-case JSONL interface, rather than app-server's camel-case items.

Regenerate app-server fixtures with `just app-server-schema-regenerate <owner>`
and check them with `just app-server-schema-check`. Experimental methods and
fields depend on the client's experimental capability and the selected schema
generation mode; consult the method registry before depending on them.

## Host filesystem operations

The `fs/*` methods operate on the configured local host filesystem using absolute
paths. They are client filesystem operations, outside a model turn's sandbox;
there is no thread or remote-environment selector in this API. Agent tool calls
obtain their sandbox context from their turn separately.

`fs/readFile` returns base64 contents up to 10 MiB and rejects a file that changes
during the bounded read. `fs/writeFile` accepts at most 10 MiB of decoded data and
rejects oversize data before modifying the target. `fs/getMetadata` returns file
kind, symlink status, byte size, and creation/modification timestamps.
`fs/readDirectory` returns names and file/directory flags; it does not return
symlink status. `fs/watch` subscriptions are connection-scoped.

The existing defaults are operation-specific: `fs/createDirectory` creates
parents, `fs/remove` recursively removes directories and ignores missing paths,
and `fs/copy` requires `recursive: true` for directories. Set `recursive: false`
and `force: false` explicitly on removal when those effects are unwanted. Exact
fields are defined in the [filesystem contract](../app-server-protocol/src/protocol/v2/fs.rs).

The internal `Op::Review` API is not exposed by the exec/app-server client.
See the [core protocol](protocol_v1.md) for the in-process API and the
[MCP interface](codex_mcp_interface.md) for the separate `codex mcp-server` service.
