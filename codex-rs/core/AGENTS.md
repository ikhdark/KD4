# Core runtime policy

This file applies inside `codex-rs/core` and inherits `codex-rs/AGENTS.md`.

## Core boundary

- Keep `codex-core` focused on session and turn orchestration, model-request and
  context assembly, tool execution flow, approvals and sandbox flow, and runtime
  event handling.
- Before adding an independent concept or public API to core, follow the owner in
  [`../../SOURCEMAP.md`](../../SOURCEMAP.md). Shared wire types, app-server
  behavior, configuration, tool contracts, and persisted state normally belong
  to their existing crates rather than core.

## Model-visible context invariants

- Build history incrementally, preserve cache-friendly stable prefixes, and
  hard-bound every injected item. No individual item may exceed 10K tokens;
  treat a new item that can exceed 1K tokens as P0 review scope.
- Define injected fragments under `src/context` as structs implementing
  `ContextualUserFragment`.
- Pre-sampling estimates must cover every item that will become model-visible
  before the request and must not consume state, mutate history, emit warnings,
  or advance extension state.
- Concurrent context contributors must still apply fragments in registration
  order. Estimate mode stays side-effect-free; runtime mutation must synchronize
  shared state.
- Keep startup prewarm and the first normal turn on the same versioned stable
  prompt-prefix cache key, preserve guardian/review overrides, and never reuse
  response IDs or turn-scoped transport state across turns.
- Telemetry-only readiness or exploration work must remain behavior-neutral and
  model-invisible unless the accepted task explicitly changes steering or
  control flow.
- Tool-search breadth changes must preserve requested limits, result schemas,
  deterministic ordering, and permission, sandbox, approval, patching, and
  execution-safety behavior.

## Diagnostics and compatibility

- Request diagnostics may record identifiers, endpoint or transport names,
  status and auth error codes, retry actions, and correlation IDs. Never record
  prompts, response bodies, tool payloads, secrets, tokens, or raw auth headers.
- Record retry or fallback cause and action where the decision occurs. Runtime
  behavior must not depend on telemetry or rollout-trace export succeeding.
- Trace core changes through app-server APIs, CLI parameters, configuration, and
  rollout/session resume compatibility before declaring them non-breaking.

## First-pass closure for tools and caches

- For a tool with a non-generic payload, trace the direct router, nested or
  code-mode dispatch, registry and exposure, lifecycle and projection mapping,
  handler, and output consumer. Function-shaped routes must share the existing
  payload conversion owner; cover valid and malformed payloads on both direct
  and nested routes instead of duplicating conversion logic.
- For cache changes, enumerate the applicable states before editing: stable hit,
  dependency drift, missing input, read failure and retry, exact identity
  mismatch, and rename, copy, or multi-path effects. Add focused proof for each
  applicable state, but do not introduce a generalized cache abstraction solely
  to satisfy this checklist.

## Core validation delta

- Agent-logic changes require focused integration coverage for major behavior.
  Prefer the existing `core/suite` and `test_codex` helpers; keep unit tests in
  dedicated `*_tests.rs` files when integration coverage is not the right level.
- Prefer `core_test_support::responses` and structured request assertions over
  manual JSON digging in core end-to-end coverage.
- Core behavior intended for Codex Desktop still requires the root publish and
  restart proof; Rust tests alone are insufficient.
