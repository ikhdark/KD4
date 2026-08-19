# Core runtime policy

## Core boundary

- Keep `codex-core` focused on session and turn orchestration, model-request and
  context assembly, tool execution flow, approvals and sandbox flow, and runtime
  event handling.
- Follow [`../../SOURCEMAP.md`](../../SOURCEMAP.md) before adding concepts or
  public APIs. Shared wire types, app-server behavior, config, tool contracts,
  and persisted state normally stay in their existing crates.

## Model-visible context invariants

- Build history incrementally with stable prefixes and hard bounds. No injected
  item may exceed 10K tokens; new items above 1K are P0 review scope.
- Define injected fragments under `src/context` as structs implementing
  `ContextualUserFragment`.
- Pre-sampling estimates must cover every item that will become model-visible
  before the request and must not consume state, mutate history, emit warnings,
  or advance extension state.
- Apply concurrent fragments in registration order; estimates stay
  side-effect-free and runtime mutations synchronize shared state.
- Keep prewarm and the first turn on the same versioned prompt-prefix key,
  preserve guardian/review overrides, and never reuse turn-scoped response or
  transport state.
- Telemetry-only readiness or exploration work must remain behavior-neutral and
  model-invisible unless the accepted task explicitly changes steering or
  control flow.
- Tool-search changes must preserve limits, schemas, deterministic ordering,
  permissions, sandboxing, approvals, patching, and execution safety.

## Diagnostics and compatibility

- Diagnostics may record identifiers, endpoint/transport names, status/auth
  codes, retry actions, and correlation IDs—never prompts, bodies, payloads,
  secrets, tokens, or raw auth headers.
- Record retry or fallback cause and action where the decision occurs. Runtime
  behavior must not depend on telemetry or rollout-trace export succeeding.
- Trace core changes through app-server APIs, CLI parameters, configuration, and
  rollout/session resume compatibility before declaring them non-breaking.

## Core validation delta

- Major agent-logic changes require focused integration coverage. Prefer
  `core/suite` and `test_codex`; otherwise use dedicated `*_tests.rs` files.
- Prefer `core_test_support::responses` and structured request assertions over
  manual JSON digging in core end-to-end coverage.
- Desktop behavior still requires root publish/restart proof.
