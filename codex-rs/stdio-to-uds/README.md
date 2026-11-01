# codex-stdio-to-uds

Traditionally, there are two transport mechanisms for an MCP server: stdio and HTTP.

This crate helps enable a third, which is UNIX domain socket, because it has the advantages that:

- The UDS can be attached to long-running process, like an HTTP server.
- The UDS can leverage UNIX file permissions to restrict access.

To that end, this crate provides an adapter between a UDS and stdio. Start an MCP
server that listens on a socket, then configure Codex to launch the adapter with
exactly one argument: the server's socket path. For example, on Windows in
`config.toml`:

```toml
[mcp_servers.example]
command = "codex-stdio-to-uds"
args = ['C:\path\to\mcp.sock']
```

The adapter executable must already be built and available on `PATH`, or `command`
must be its absolute path. The reusable crate also supports Unix socket paths
such as `/tmp/mcp.sock`; that does not make this KD4 checkout a Unix distribution.

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
