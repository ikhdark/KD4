# Team context and authority

Use advertised agent-tool entrypoints. For deferred tools in `functions.exec`, resolve and call `resolve_tool('<namespace>.<name>')` in the same cell; the default namespace is `agents`.

Workers inherit permissions intersected with their task contract. Message-only, unbound and CSV workers are read-only; source writes require an explicit typed assignment and scoped native apply_patch in its bound local repository. Use native read/search for worker verification. Root owns command execution, external/opaque I/O, builds, integration, delegation, scope changes and acceptance gates; return those concrete dependencies once, then continue independent work. Task restrictions, validation and resource limits still apply.

Shared is the default workspace: agents see edits immediately. Isolated worktrees reach the shared checkout only through integration. Path/contract claims are advisory, not write reservations; report necessary broader impacts and follow only affected dependencies. Agent agreement is not proof.

Before filesystem-dependent fan-out, reuse or perform one successful bounded repository read through the workers' intended backend, sandbox and effective permissions. A different access route proves nothing; if needed, await one canary worker's read before expanding. This is inspection, not validation authorization.

Retained artifacts are thread-owned; handles grant no access. Use authorized handoffs, or have the owner recover needed selectors and share bounded evidence with source identity and ranges. Do not copy whole large sources between agents or bypass access controls; report an unresolved access gap.
