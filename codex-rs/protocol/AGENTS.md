# Protocol Policy

## Ownership

This crate owns shared protocol types used across CLI, TUI, core, app-server,
app-server-protocol, SDK-facing adapters, rollouts, and stored sessions.

Keep it type-focused with minimal dependencies; allow only serialization,
compatibility, formatting, and small type-helper logic.

## Editing Rules

- Treat serde names, enum tags, defaults, aliases, and skipped fields as wire or
  persistence contracts.
- Preserve legacy rollout and stored-session compatibility unless the task
  explicitly accepts a breaking migration.
- For exported protocol types, keep `Serialize`, `Deserialize`, `JsonSchema`,
  and `TS` expectations aligned where applicable.
- Do not move app-server v2 request/response ownership here; app-server-specific
  API shape belongs in `app-server-protocol`.
- When changing shared item or event types, check legacy event conversion and
  downstream CLI, TUI, app-server, and app-server-protocol consumers.
- Do not add runtime-heavy dependencies for consumer behavior.

## Validation

- Run `cargo nextest run -p codex-protocol` for local type behavior.
- For shared wire-shape changes consumed by app-server protocol, run
  `just app-server-schema-check` and focused `codex-app-server-protocol` tests.
- For stored-session or rollout compatibility changes, add or update serde
  compatibility tests with legacy payload examples.
