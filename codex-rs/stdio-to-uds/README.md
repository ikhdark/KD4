# codex-stdio-to-uds

Traditionally, there are two transport mechanisms for an MCP server: stdio and HTTP.

This crate helps enable a third, which is UNIX domain socket, because it has the advantages that:

- The UDS can be attached to long-running process, like an HTTP server.
- The UDS can leverage UNIX file permissions to restrict access.

To that end, this crate provides an adapter between a UDS and stdio. The idea is that someone could start an MCP server that communicates over `/tmp/mcp.sock`. Then the user could specify this on the fly like so:

```
codex --config mcp_servers.example={command="codex-stdio-to-uds",args=["/tmp/mcp.sock"]}
```

Unfortunately, the Rust standard library does not provide support for UNIX domain sockets on Windows today even though support was added in October 2018 in Windows 10:

https://github.com/rust-lang/rust/issues/56533

As a workaround, this crate uses `codex-uds`, which provides a cross-platform async UDS API backed by https://crates.io/crates/uds_windows on Windows.

## Completion policy

The adapter is process-scoped and may be run only once. Native stdio workers
are detached from the async runtime, with bounded chunk staging; cancellation
does not wait for a blocked OS read or write. Stdin EOF half-closes socket writes
and continues reading responses. Socket read EOF ends the session after stdout
is physically flushed, even if stdin remains open.

An input failure is retained while responses drain for at most five seconds;
each native stdout write/flush also has a five-second stall limit. Expiry or a
failed sink returns an explicit incomplete-delivery error, never flushed success.
