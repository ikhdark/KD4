# KD4 fork policy

This checkout is the user's local fork of OpenAI Codex at
`C:\Users\kuh\Desktop\kd4`. Treat work here as fork-local unless the user
explicitly asks for upstream, product-facing, or distribution-ready changes.

The standing objective is to improve, audit, and optimize this checkout while
keeping changes reviewable, local-build focused, and easy to validate.

## Current instruction layout

This repo currently has this root `AGENTS.md`, nested `.codex/AGENTS.md`,
`codex-rs/AGENTS.md`, scoped nested `AGENTS.md` files under `codex-rs` where
present, and `scripts/AGENTS.md` for script tooling.
Do not refer to nested `AGENTS.md` files unless they are present in the working tree.

Repo-local Codex guidance and workflows live under `.codex` when present,
especially:

- `.codex/AGENTS.md` and `.codex/README.md`: repo-local Codex setup and
  policy.
- `.codex/config.toml`: optional repo-local Codex runtime configuration.
- `.codex/environments`: generated/local environment setup.
- `.codex/skills`: fork-local skills for crosscheck-and-finish implementation
  discipline in this checkout.

If durable guidance becomes specific to one package or directory, add a nested
`AGENTS.md` there instead of expanding this root file.

## Default workflow

- For top-N, ranking, brainstorm, optimization-list, review, recommendation, or
  "what would you fix" requests, return findings or ranked candidates first and
  do not edit until the user clearly asks for implementation.
- When implementing, ignore unrelated dirty-worktree changes, untracked files,
  generated artifacts, and failures outside the accepted task scope.
- If code changes overlap with existing local edits, stop and compare the
  versions. Keep or add the better version, integrate compatible improvements
  where practical, and then continue without reverting unrelated work.
- Verify drift-prone repo-state facts before relying on them, including remotes,
  upstream tracking, current branch, installed binary paths, available recipes,
  generated artifact freshness, and desktop/runtime process paths.
- Prefer focused source changes with matching focused validation. Avoid mixing
  cleanup, behavior changes, dependency changes, and generated artifact updates
  unless one requires the other.
- Preserve upstream-compatible behavior unless the user explicitly wants
  local-only fork behavior. Call out changes that affect public CLI flags,
  app-server APIs, config loading, sandbox behavior, stored sessions, rollout
  compatibility, or installed-binary behavior.
- Do not touch patch/apply_patch guards, stale-read or preflight behavior,
  approval, permission, sandbox, validation, test-gating, or execution-safety
  behavior as part of unrelated work unless the user names that safety surface.

## Task lanes

Choose the lightest lane that can safely satisfy the request. When in doubt,
start with the lighter lane for inspection, but escalate immediately if
inspected evidence shows runtime, wiring, schema, safety, generated-artifact, or
ownership risk. Before non-trivial implementation work, state the selected lane
and validation intent briefly.

- Conversation lane: casual Q&A, explanation, brainstorming, and non-coding
  prompts. Do not inspect the repo unless the answer depends on current files or
  repository behavior.
- Low-risk guidance lane: documentation, instructions, comments, naming, and
  small non-runtime cleanup. Inspect the target file plus the nearest applicable
  `AGENTS.md`; validate with focused diff review and `git diff --check` when
  whitespace risk exists. If the work touches source behavior, routing, config,
  schema, tests, generated files, or tool behavior, escalate.
- Focused code lane: narrow implementation, debugging, refactoring, integration,
  migration, code review, code audit, configuration, or repository-behavior work
  with a clear owner and limited blast radius. Inspect the owner, nearest call
  path, relevant config or docs, and nearest tests before editing. Validate with
  the smallest focused check that proves the touched behavior.
- Runtime-critical lane: changes involving safety-sensitive surfaces,
  generated artifacts, lockfiles, protocol/schema/shared contracts,
  desktop-visible behavior, publish/install paths, sandbox/approval/test-gating,
  dependency changes, broad refactors, unclear ownership, or runtime/wiring
  risk. Use the full crosscheck, local-build, generated-artifact, publish, or
  runtime proof required by the affected surface.

## Coding depth guard

Use this guard only for implementation, debugging, refactoring, code review,
code audit, integration, migration, configuration, or repository-behavior tasks.
Do not apply it to casual Q&A, explanations, brainstorming, or non-coding prompts
unless the user explicitly asks for a rigorous pass.

- Do not answer from memory when the task depends on repository behavior.
- Inspect the relevant files, call paths, configs, tests, and runtime entrypoints
  before making implementation or review claims.
- Externalize key assumptions, invariants, inspected evidence, and validation
  intent when they affect correctness.
- For implementation work, prove the changed code is reachable through the
  intended runtime path and that stale code does not still win.
- For bug fixes, identify the failing path and the nearest validation that
  exercises the corrected behavior.
- For reviews and audits, state the inspected scope and avoid claims that exceed
  that scope.
- For broad claims such as "all", "every", "complete", or "repo-wide", perform and cite an appropriate closure sweep. If the sweep is partial, state the exact inspected scope and remaining risk instead of implying broader coverage.
- Do not claim a change is complete after a quick skim, partial implementation,
  compile-only check, or unverified generated output. If evidence is incomplete,
  say exactly what remains unverified.

## Local build proof

Prove local Codex build work on this checkout only. Prioritize:

- the relevant Rust workspace or crates compile;
- the local `codex` binary builds;
- local publish or dry-run paths succeed when touched;
- installed binary replacement is correct when touched;
- desktop/app-server runtime uses the local build after rebuild, publish, and
  restart when desktop-visible behavior changes;
- focused tests for touched crates pass.

Do not spend time on broad CI, upstream release polish, public distribution
compatibility, unrelated failures, or repo-wide cleanup unless it directly
blocks the accepted local-build goal. Validation tooling passing does not prove a
runtime bug is fixed; only claim runtime fixes after the focused failing test or
approved final gate passes.


## Tool Use

- Before non-trivial tool work, send one brief preamble grouping the next actions.
- Use `rg` or `rg --files` for plain text and file-name discovery when
  available.
- Prefer installed purpose-built local tools when they are present and clearly
  fit the task: prefer `fd` for file discovery, `jq`/`yq` for JSON/YAML
  inspection, `delta` for readable Git diffs, `ast-grep` for syntax-aware search, `just`
  recipes before lower level build/test/publish commands, `cargo
  nextest`/`cargo shear`/`cargo audit`/`cargo deny` for Rust test and
  dependency-health workflows when applicable, `taplo`/`dprint` for configured
  formatting or validation, and `hyperfine`/`tokei` for performance and sizing
  measurements.
- Use `apply_patch` for manual edits.
- Never publish or commit unless asked.
- Do not re-read large files unnecessarily after a successful patch; inspect
  targeted diffs instead.


## Desktop app boundary

The user uses the Windows Codex desktop app by default. This repo contains Rust
CLI and app-server components the desktop can use, but not the native Windows
desktop shell source.

Source edits here do not hot-apply to the installed app. Desktop-visible changes
require rebuilding and updating or replacing the relevant local binary. Use the
`targetPath` printed by the local publish script. On this machine,
`CODEX_LOCAL_PUBLISH_DIR` is `C:\Users\kuh\Desktop\LOCAL-KD`, so the expected
publish target is `C:\Users\kuh\Desktop\LOCAL-KD\codex.exe`, followed by a
Codex Desktop restart.

Desktop-visible completion requires evidence of the runtime chain: running
desktop process path, local binary hash/version, app-server initialize/model
metadata when relevant, and a user-visible screenshot or equivalent runtime
evidence. Final status for desktop changes must state whether visibility still
requires `just publish-local-codex-final` and a desktop restart.

## Validation map

Use this map when validation is requested, needed for local-build proof, or not
explicitly waived. If the user says no tests, do not run test commands.

Prefer the nearest sufficient proof: run the smallest focused command or runtime
check that proves the touched behavior through its normal owner or entrypoint.
Do not stack build, test, lint, publish, and runtime checks unless each proves a
distinct required claim. Broad validation is required only for shared contracts,
dependency or workspace configuration, generated artifacts,
publish/install behavior, or desktop-visible runtime behavior.

- Rust crate changes: run the focused crate check/test from `codex-rs`, using a
  local `just` recipe when one exists.
- App-server schema or protocol changes: run focused app-server tests and
  regenerate schema artifacts with `just write-app-server-schema` when the wire
  contract changed.
- Config schema changes: run focused config/core tests and regenerate
  `codex-rs/core/config.schema.json` with `just write-config-schema`.
- Python SDK changes: use the SDK's focused `uv run pytest`,
  `uv run ruff check .`, lock checks, and artifact regeneration only for touched
  SDK surfaces.
- Script changes: run syntax checks and the closest script tests when present;
  do not hand-edit generated locks such as `scripts/uv.lock`.
- Root maintenance changes: prefer root `package.json` scripts when they match
  the touched surface.
- Local publish changes: use `just publish-local-codex-dry-run` for path/proof
  changes and `just publish-local-codex-final` before claiming installed local
  binary replacement.

For protocol, app-server, SDK, config-schema, generated-artifact, or publish-path
changes, identify the owning contract before editing implementation code. Update
generated outputs only through the owning generator or recipe.

## Current surface map

- `.github`: repository automation.
- `.codex`: repo-local Codex config, environments, and skills.
- `bazel`, root Bazel files, and `rbe.bzl`: Bazel modules, rules, dependency
  pins, toolchains, platforms, patches, and lock state.
- `codex-rs`: primary Rust workspace for CLI, core runtime, app-server,
  protocol, TUI, exec, extensions, plugins, skills, memory, config, sandboxing,
  process, and helper crates.
- `codex-cli`: npm-facing `@openai/codex` wrapper, native binary discovery, npm
  package staging, and install/update behavior.
- `docs`: checked-in user, contributor, and behavior documentation.
- `patches`: Bazel patch inputs for third-party dependencies and platform
  builds. Keep patch exports synchronized with referenced patches.
- `scripts`: repository tooling, local publish/build helpers, packaging,
  install scripts, schema utilities, and validation helpers.
- `sdk`: Python, Python runtime, and TypeScript SDK surfaces.
- `third_party`: vendored or checked-in third-party integration inputs.
- `tools`: repository tooling outside the Rust workspace, including
  `tools/argument-comment-lint`.

## No-edit zones

Do not hand-edit generated files, vendored code, lockfiles, Bazel metadata, or
build outputs unless the source change requires regeneration or an owning
workflow requires the update. In particular, do not hand-edit files under:

- `codex-rs/target`
- `node_modules`
- `codex-rs/vendor`
- `third_party`
- `codex-rs/app-server-protocol/schema`
