# Codex as an MCP server

`codex mcp-server` exposes Codex through the standard Model Context Protocol
(MCP) over stdio. The interface is experimental.

## Start the server

Run either command:

```bash
codex mcp-server
```

```bash
cargo run -p codex-mcp-server
```

An MCP client must perform the normal MCP initialization handshake before it
lists or calls tools.

## Tools

The server publishes exactly two Codex tools through `tools/list`:

### `codex`

Starts a new Codex thread and runs its first prompt.

Required argument:

- `prompt`: the initial user prompt.

Optional arguments include `model`, `cwd`, `approval-policy`, `sandbox`,
configuration overrides, base and developer instructions, and a compact
prompt. The JSON schema returned by `tools/list` is authoritative for exact
field names and allowed values.

### `codex-reply`

Continues an existing thread.

Required arguments:

- `threadId`: the thread ID returned by a previous tool call; and
- `prompt`: the next user prompt.

The deprecated `conversationId` argument is still accepted for compatibility,
but new clients should use `threadId`.

## Results

Both tools return MCP text content. The same text is mirrored in
`structuredContent.content`, and the thread identifier is returned as
`structuredContent.threadId`. A successful response has this shape:

```json
{
  "content": [
    { "type": "text", "text": "..." }
  ],
  "structuredContent": {
    "threadId": "...",
    "content": "..."
  }
}
```

Some results can also include `structuredContent.surfacedResult`.

When Codex needs approval or another interactive response, the server can send
standard MCP elicitation requests (`elicitation/create`). Clients using an
approval policy that prompts must support and answer those requests; general
elicitation requests are cancelled when the client lacks the required
capability.

## MCP is not the app-server protocol

App-server JSON-RPC methods such as `thread/start`, `turn/start`, account
methods, configuration methods, and model-list methods are not exposed by
`codex mcp-server`; custom requests that are not part of its MCP surface return
method-not-found. Use `codex app-server` and the
[app-server protocol documentation](../app-server/README.md) for those APIs.

Similarly, `codex mcp` manages external MCP server launchers in Codex
configuration. It does not call the `codex mcp-server` tools described here.

## Implementation references

- [`message_processor.rs`](../mcp-server/src/message_processor.rs) owns MCP
  request dispatch and the published tool list.
- [`codex_tool_config.rs`](../mcp-server/src/codex_tool_config.rs) owns the two
  tool schemas.
- [`codex_tool_runner.rs`](../mcp-server/src/codex_tool_runner.rs) owns tool
  execution and result formatting.

For client setup and current MCP configuration guidance, see the
[official Codex MCP documentation](https://developers.openai.com/codex/extend/mcp).
