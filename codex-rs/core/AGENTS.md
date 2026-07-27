# Core runtime policy

This file applies inside `codex-rs/core`. It inherits the Rust workspace rules
from `codex-rs/AGENTS.md`.

## The `codex-core` crate

Over time, the `codex-core` crate has become bloated because it is the largest
crate, so it is often easier to add something new to `codex-core` rather than
refactor out the library code you need so your new code neither takes a
dependency on, nor contributes to the size of, `codex-core`.

To that end: **resist adding code to codex-core**.

Particularly when introducing a new concept, feature, or API, before adding to
`codex-core`, consider whether:

- There is an existing crate other than `codex-core` that is an appropriate
  place for your new code to live.
- It is time to introduce a new crate to the Cargo workspace for your new
  functionality. Refactor existing code as necessary to make this happen.

Likewise, when reviewing code, do not hesitate to push back on PRs that would
unnecessarily add code to `codex-core`.

Before adding new core code, check whether the owner is actually another crate:

- `protocol/` for shared request/event/session types crossing clients.
- `app-server/` for desktop/IDE request preprocessing, scheduling, and API
  behavior.
- `config/` for config loading, permissions, profiles, feature flags, and
  schema.
- `tools/` for tool definitions, schemas, and tool result contracts.
- `state/` for persisted runtime state, logs, goals, memories, and thread
  metadata.

Keep `core/` focused on session orchestration, turn loop behavior, model request
assembly, context management, tool execution flow, sandbox/approval flow, and
runtime event handling.

## Code Review Rules

### Model visible context

Codex maintains a context, or message history, that is sent to the model in
inference requests.

1. No history rewrite: the context must be built up incrementally.
2. Avoid frequent changes to context that cause cache misses.
3. No unbounded items: everything injected in the model context must have a
   bounded size and a hard cap.
4. No items larger than 10K tokens.
5. Highlight new individual items that can cross more than 1K tokens as P0.
   These need an additional manual review.
6. All injected fragments must be defined as structs in `core/context` and
   implement `ContextualUserFragment`.

### Pre-turn token estimates

- Keep pre-sampling compaction estimates aligned with everything that will be
  model-visible before the next sampling request: pending user input, context
  updates, session-memory context items, and explicit skill/plugin/extension
  turn-input injections.
- Token estimation must be side-effect-free. Use peek or estimate-only paths so
  preflight accounting does not consume memory, mutate history, emit warnings, or
  advance extension state.
- When estimation order differs from recording order for cache or compaction
  reasons, add focused coverage showing later-recorded items are included in the
  estimate without being consumed.

### Prompt assembly and prewarm cache

- `Session::build_initial_context_inner` owns the stable prompt prefix assembled
  before turn-local user input. Context contributors may be awaited in parallel
  for latency, but their fragments must still be applied in contributor
  registration order so model-visible prompt order remains deterministic.
- Keep `ContextContributionMode::Estimate` side-effect-free. Contributors that
  mutate extension state must do so only in runtime mode and must synchronize
  shared state because prompt assembly can poll multiple contributors
  concurrently.
- Normal model-client sessions should use the versioned stable prompt-prefix
  cache key, and startup prewarm plus the first regular turn must share that key.
  Preserve guardian/review-session prompt-cache overrides.
- Do not reuse provider response ids, websocket turn state, or other turn-scoped
  transport state across turns when changing prompt-prefix prewarm behavior.

### Turn-local readiness context

- Keep validation freshness advisory and factual. Formatting-only commands may be
  reported distinctly, but they must not clear correctness validation freshness.
- Keep exploration-budget work behavior-neutral unless the accepted task
  explicitly asks for model-visible steering or control-flow changes. Prefer
  trace telemetry and bounded tests over stop hooks, model-visible nudges,
  deadline cutoffs, tool-call caps, search-result clamps, or completion blockers.
- Tool-search breadth changes should preserve requested limits and schema
  contracts. If diversifying results, use deterministic ordering within the
  existing result shape and avoid permission, sandbox, approval, patching, or
  execution-safety surfaces.
- Do not add model-visible budget, shallow-evidence, deadline, or continue/stop
  advisory fragments as part of telemetry-only work. Add them only for an
  explicitly accepted model-visible context change.

### Request diagnostics and transport evidence

- `core/src/client.rs` owns model request transport wiring for HTTP Responses,
  Responses WebSocket, auth recovery retries, and rollout trace attempt
  correlation. Keep diagnostic additions on that path body-safe: request ids, CF
  ray ids, auth error codes, endpoint names, transport names, status codes, retry
  actions, and inference call ids are acceptable; prompts, response body text,
  tool payloads, API keys, tokens, or raw auth headers are not.
- When adding fallback or retry behavior, record the causal chain where the
  decision is made: transport, trigger, cause, action, status, and available
  request/debug ids. Diagnostics should explain existing behavior and avoid
  unrelated control-flow changes.
- Keep request telemetry and rollout tracing correlated without making rollout
  tracing mandatory. Disabled trace attempts must continue to emit no
  inference-call id, and normal request execution must not depend on telemetry
  export success.

### Breaking changes

Search for breaking changes in external integration surfaces:

- app-server APIs
- CLI parameters
- configuration loading
- resuming sessions from existing rollouts

Core behavior changes that should be visible in Codex Desktop still require the
local binary publish/restart path from the root `AGENTS.md`; source diffs and
Rust tests alone do not prove desktop-visible behavior.

### Test authoring guidance

For agent changes, prefer integration tests over unit tests. Integration tests
are under `core/suite` and use `test_codex` to set up a test instance of Codex.

Features that change the agent logic must add or identify focused integration
test coverage for the major logic changes and user-facing behaviors.

If the user says no tests, this still means add or identify the appropriate
focused test coverage when the change needs it, but do not run
Rust/Cargo/nextest test commands in that turn. Report the skipped test command
for the user to run.

If unit tests are needed, put them in a dedicated test file named `*_tests.rs`.
Avoid test-only functions in the main implementation.

Check whether there are existing helpers to make tests more streamlined and
readable.

### Change size guidance

Unless the change is mechanical, the total number of changed lines should not
exceed 800 lines. For complex logic changes, keep the size under 500 lines.

If the change is larger, explore whether it can be split into reviewable stages
and identify the smallest coherent stage to land first. Base the staging
suggestion on the actual diff, dependencies, and affected call sites.
