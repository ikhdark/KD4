# Protocol Policy

This file applies inside `codex-rs/protocol`. It inherits `codex-rs/AGENTS.md`.

## Ownership

This crate owns shared protocol types used across CLI, TUI, core, app-server,
app-server-protocol, SDK-facing adapters, rollouts, and stored sessions.

Keep this crate type-focused with minimal dependencies. Avoid material business
logic here unless it is required for serialization, compatibility, formatting, or
small type helpers.

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
- Keep dependencies minimal; do not add runtime-heavy dependencies for behavior
  that belongs in a consuming crate.
- Before changing a serializable struct or enum, close its data shape: find all
  direct constructors and destructuring sites, readers and writers, stored
  fixtures, and generated schema or SDK representations. Additive persisted
  fields must define the intended missing-field behavior and include both a
  legacy payload test and a new-shape round-trip test.

## Validation

- For local type behavior, run `cargo nextest run -p codex-protocol`.
- For shared wire-shape changes consumed by app-server protocol, run
  `just app-server-schema-check` and focused `codex-app-server-protocol` tests.
- For stored-session or rollout compatibility changes, add or update serde
  compatibility tests with legacy payload examples.
