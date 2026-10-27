# Exec-server transport authority

Prefer `codex exec-server --listen stdio` for a private parent-child connection.
Stdio does not open a listener or require a capability token.

The standalone WebSocket listener remains loopback-only. Before launching it,
the parent must generate a fresh, unpredictable per-launch capability (at least
256 random bits encoded as URL-safe ASCII) and pass it privately in
`CODEX_EXEC_SERVER_AUTH_TOKEN` to both the server and authorized client processes.
Do not put the token in command-line arguments, URLs, logs, or persistent/shared
environment settings. The listener refuses to start without a token of at least
32 URL-safe ASCII characters; this length check does not guarantee randomness.

Native clients must send exactly one `Authorization: Bearer <token>` header.
Authentication precedes WebSocket upgrade, capacity admission, and all new or
resumed JSON-RPC sessions. The server prints only the loopback URL. Origin
headers remain forbidden. `/readyz` exposes readiness only, not RPC authority.

The built-in URL client sends this process-scoped capability only to numeric
loopback IP endpoints (`127.0.0.1` or `[::1]`, including other loopback addresses),
never to a remote hostname. Reconnect uses the same admission rules. Configure
an explicitly direct or bypassed route: capability-bearing setup
refuses a selected proxy before any TCP/CONNECT write instead of silently bypassing
the effective proxy policy. Private stdio is the alternative when policy cannot
permit a local route. Handshake diagnostics retain byte counts, not request bytes.
The launcher must not grant the capability to a less-trusted process: possession authorizes
the server account's filesystem and process operations. This capability is
separate from remote Noise registry credentials and Remote/mobile enrollment.

Runtime callers should construct `EnvironmentManager` with its effective
`HttpClientFactory` variants. The factory is retained through startup, URL and
Noise reconnects, registry HTTP, and executor rendezvous connections; direct API
clients supply it in their connection arguments. Legacy constructors explicitly
retain environment-proxy defaults when no application configuration is supplied.
