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

## Capability manifests

`initialize` returns an optional `capabilityManifest`; initialized clients can
refresh it with `environment/capabilities`. Version 1 binds the backend name,
process incarnation and package version, an opaque runtime-configuration digest
(platform, architecture, cwd and helper executable), and the router's sorted
registered request methods. No credentials or permission grants enter the manifest.

`environment/capabilities/query` accepts `{"methods":["fs/readFileRange"]}`
and returns a versioned report with the manifest and an `operations` map of
`supported`, `unsupported`, or `unknown`. Empty methods enumerate known methods;
queries accept at most 64 exact IDs of 1-128 bytes. Missing methods are unsupported
only when `methodsComplete` is true; partial inventories and absent legacy
metadata mean unknown. Existing version-1 RPC inventories are complete by default.

The read-only `backend_capabilities` tool refreshes the selected environment,
without host fallback. Native environments report implemented equivalents as a
partial inventory; RPC-only methods are not inferred. Its configuration revision
also binds the selected cwd and sandbox configuration, without exposing policy
details or granting authority. `Environment::capability_report` refreshes the
same owner; legacy method-not-found means unknown, not unsupported. No report is
persisted or used to bypass the owning operation's live support/permission checks.

Native clients attach its `revision` as request-level `capabilityRevision`.
The server revalidates before admission-time effects and again after queued
dependencies, before executing an operation. A mismatch returns `-32012` with
`effects: "none"`; refresh discovery and reconsider the operation, never silently
replay it. Reconnect acquires a new transport-bound snapshot. Resource reads,
termination/cancellation and handle release remain usable without an expected
revision, under their existing session and ownership checks.

Supported does not mean permitted: this is method-level support, not a guarantee
for every parameter, sandbox, resource or caller. Initialization, authentication,
sandbox policy and process incarnation checks still apply. Legacy peers without
a manifest retain method dispatch checks; they provide no versioned support
evidence, and unsupported methods must not fall back to a weaker operation.
