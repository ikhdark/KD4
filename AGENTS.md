# Repository policy

Shared policy revision: `2026-07-12`.

## Synchronization contract

This root policy is synchronized with:

- `C:\Users\kuh\Desktop\kd4\AGENTS.md`
- `C:\Users\kuh\Desktop\KDWG\AGENTS.md`
- `C:\Users\kuh\Desktop\mdpwa-main\AGENTS.md`
- `C:\Users\kuh\Desktop\kds-main\AGENTS.md`

Every byte outside the project-context block delimited below must remain
identical across all four files. Only that block may contain repository-specific
identity, ownership, commands, validation, runtime, safety, install, or protected
path details.

When a shared rule changes, update all four files in the same task, keep the
shared policy revision aligned, and compare normalized copies after replacing
each project-context block with the same sentinel. Never add a repository-specific
exception outside that block or change only one shared copy.

## Project context

<!-- PROJECT-CONTEXT: START (only this block may differ) -->
### Identity and objective

- This checkout is the user's local fork of
  [`openai/codex`](https://github.com/openai/codex) at
  `C:\Users\kuh\Desktop\kd4`. Its home repository is
  [`ikhdark/KD4`](https://github.com/ikhdark/KD4).
- KD4 is a modified Codex for the user's improvements, experiments, local
  workflows, and fork-specific tooling. Treat work as fork-local unless the
  user explicitly requests upstream, product-facing, or distribution-ready
  changes.
- The standing objective is to improve, audit, and optimize the checkout while
  keeping changes reviewable, local-build focused, and easy to validate.

### Source of truth and ownership

- Known top-level instruction scopes include `.codex/AGENTS.md`,
  `codex-rs/AGENTS.md`, and `scripts/AGENTS.md`; further nested files apply
  only where present.
- `.codex/AGENTS.md` and `.codex/README.md` own workspace policy and routing.
  `.codex/config.toml` owns optional repo-local runtime configuration,
  `.codex/environments` owns worktree setup/state, and `.codex/skills` owns
  fork-local skills and validation workflows.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns the high-level directory map, runtime
  entrypoints, Rust-domain routing, build/package/publish paths, generated
  contracts, and cross-cutting change routes.

### Project constraints

- Preserve upstream-compatible behavior unless the user explicitly wants
  local-only fork behavior. Call out changes affecting public CLI flags,
  app-server APIs, configuration loading, sandbox behavior, stored sessions,
  rollout compatibility, or installed-binary behavior.
- Identify the owning contract before editing protocol, app-server, SDK,
  configuration-schema, generated-artifact, or publish-path behavior. Update
  generated outputs only through the owning generator or recipe.
- The repository contains the Rust CLI and app-server components used by Codex
  Desktop, but not the native Windows desktop shell source. Source edits do not
  hot-apply to the installed app.

### Validation and runtime proof

- Rust changes: work from `codex-rs` and prefer the focused crate `just` recipe
  or focused Cargo check/test.
- App-server schema or protocol: run focused app-server tests and
  `just app-server-schema-check`. Use the force or raw generator recipes only
  for intentional contract regeneration.
- Configuration schema: run focused config/core tests and
  `just config-schema-check`. Use force or raw generator recipes only for
  intentional `codex-rs/core/config.schema.json` regeneration.
- Python SDK changes: use focused `uv run pytest` and `uv run ruff check .`;
  regenerate locks or artifacts only for touched SDK surfaces.
- Script changes: run syntax checks and the closest script tests. Do not
  hand-edit generated locks such as `scripts/uv.lock`.
- Root maintenance: prefer matching root `package.json` scripts.
- Local publish path changes: use `just publish-local-codex-dry-run` for path
  proof and `just publish-local-codex-final` before claiming installed
  replacement.
- For a local-build claim, prove only the applicable links: relevant crates
  compile, focused tests pass, the local `codex` binary builds, publish or
  dry-run paths succeed when touched, installed replacement is correct when
  touched, and the active desktop/app-server uses the local build after
  rebuild, publish, and restart.
- `CODEX_LOCAL_PUBLISH_DIR` is `C:\Users\kuh\Desktop\LOCAL-KD`, so the expected
  publish target is `C:\Users\kuh\Desktop\LOCAL-KD\codex.exe`. Desktop-visible
  completion also requires the running process path, local binary hash/version,
  relevant app-server initialize/model metadata, and a user-visible screenshot
  or equivalent evidence. State whether `just publish-local-codex-final` and a
  Desktop restart remain required.

### Protected paths and state

- Do not hand-edit `codex-rs/target`, `node_modules`, `codex-rs/vendor`,
  `third_party`, or `codex-rs/app-server-protocol/schema`.
<!-- PROJECT-CONTEXT: END -->

## Instruction scope

This file applies repository-wide. Before editing, discover instruction files
from the repository root with `rg --files --hidden -g AGENTS.md` and read the
closest applicable file. A nearer `AGENTS.md` augments this policy and takes
precedence within its subtree. Never rely on an instruction file that is absent
from the working tree.

Keep shared workflow rules in the synchronized portion of this root file. Keep
durable repository-specific rules inside the project-context block or the
nearest nested `AGENTS.md`. README and other documentation files are not loaded
automatically as instructions; promote operational editing rules into the
closest `AGENTS.md` and keep documentation focused on usage, architecture, and
background.

Use the ownership maps and source-of-truth documents named in the project
context when ownership is ambiguous, the task is cross-cutting, or a
runtime-to-package/install path must be traced. For a clear local owner, prefer
the nearest scoped instructions and owner documentation.

## Operating defaults

- For top-N, ranking, brainstorm, optimization-list, review, recommendation, or
  "what would you fix" requests, return findings or ranked candidates first and
  do not edit until the user clearly asks for implementation.
- When implementing, ignore unrelated dirty-worktree changes, untracked files,
  generated artifacts, and failures outside the accepted task scope.
- If task-relevant changes overlap existing local edits or duplicate
  implementations, stop and compare them. Keep or produce the stronger
  compatible path without reverting unrelated work.
- Verify drift-prone repository facts before relying on them, including remotes,
  upstream tracking, the current branch, installed paths, available recipes,
  generated artifact freshness, and active runtime or process paths.
- Prefer focused source changes with matching focused validation. Do not mix
  cleanup, behavior changes, dependency changes, generated outputs, or release
  work unless one requires another.
- Preserve established public, product, and compatibility behavior unless the
  user explicitly requests a change. Call out changes to public interfaces,
  stored data, configuration, security, install, rollout, or runtime behavior.
- Do not touch patch guards, stale-read or preflight behavior, approval,
  permission, sandbox, validation, test-gating, or execution-safety behavior as
  part of unrelated work unless the user names that surface.
- If an accepted task exposes a fixable issue that directly prevents the change
  from being complete, durable, or correctly validated, fix it within the same
  natural ownership boundary. Report broader or unrelated issues separately.
- Use durable harness, plan, log, eval, QA, handoff, or multi-agent artifacts
  only when the user explicitly requests them or a nearer instruction file
  explicitly requires them.

## Task lanes

Choose the lightest lane that safely satisfies the request, then escalate as
soon as inspection reveals higher risk. Before non-trivial implementation work,
state the selected lane and validation intent briefly.

| Lane | Use when | Minimum evidence |
| --- | --- | --- |
| Conversation | Casual Q&A, explanation, brainstorming, or non-coding prompts | Do not inspect the repository unless the answer depends on current files or repository behavior. |
| Low-risk guidance | Documentation, instructions, comments, naming, or small non-runtime cleanup | Inspect the target and nearest `AGENTS.md`, review the focused diff, and use `git diff --check` when allowed and whitespace risk exists. |
| Focused code | Narrow implementation, debugging, refactoring, integration, migration, review, audit, configuration, or repository behavior with a clear owner | Inspect the owner, nearest call path, relevant configuration or docs, and nearest tests; run the smallest allowed check that proves the touched behavior. |
| Runtime-critical | Safety surfaces, generated artifacts, lockfiles, shared protocol or schema contracts, publish/install or user-visible runtime behavior, dependencies, broad refactors, or unclear ownership | Crosscheck the complete owning path and run the applicable generated-artifact, local-build, publish/install, or runtime proof. |

## Repository evidence and change workflow

Use this workflow for implementation, debugging, refactoring, code review,
code audit, integration, migration, configuration, or repository-behavior
tasks. Do not apply it to casual Q&A unless the user requests a rigorous pass.

- Do not answer from memory when a claim depends on repository behavior.
- Identify the smallest authoritative file set, then trace one relevant hop
  through callers, registries, configuration or schema owners, tests, docs, and
  install descriptors that can affect the requested path.
- Externalize assumptions, invariants, inspected evidence, and validation intent
  when they affect correctness.
- For implementation work, update the complete normal runtime, discovery, or
  install path. Check replaced and parallel paths so stale behavior does not
  still win.
- Do not leave task-relevant TODOs, stubs, placeholders, inert registrations, or
  mismatched docs and tests in the intended path.
- For bug fixes, identify the failing path and the nearest allowed validation
  that exercises the corrected behavior.
- For reviews and audits, state the inspected scope and keep findings within it.
- For broad claims such as "all", "every", "complete", or "repo-wide", perform
  an appropriate closure sweep. If the sweep is partial, state the exact scope
  and remaining risk.

## Completion and wiring gate

Before claiming an implementation is complete, verify all of the following:

- the intended runtime or workflow path is identified and reaches the change;
- expected callers, registrations, configuration, and replaced or parallel
  paths were checked;
- no new or task-relevant placeholder or stub remains in the changed code or
  intended path;
- the nearest practical, authorized validation ran, or its skip reason is
  explicit.

When Wiring Guard/KDWG is available for implementation work, use it as the
static reachability proof layer: declare intent before implementation-shaped
edits and run its check before a completion claim. For docs, templates,
planning, packaging, or configuration-only work with no executable call path,
use the current owned docs-only opt-out or no-wiring-target mechanism with a
specific reason. Static wiring proof does not replace behavior validation.

Final implementation answers must report the completion gate (`passed`,
`partial`, or `blocked`), wiring proof, validation run, and remaining unverified
risk. Do not claim completion when the gate is partial or blocked, or when the
wiring verdict does not support it.

## Validation

Follow the repository-specific authorization and commands in the project
context. If the user or project context forbids tests, validation, browser
automation, command execution, or another check, honor that restriction and
state what was skipped. Static Wiring Guard checks remain allowed unless command
execution is also forbidden.

- Use the nearest sufficient proof. Stop when the smallest relevant checks prove
  the changed path.
- For docs or guidance, review the focused diff and use `git diff --check` when
  allowed and applicable.
- For behavior changes, prefer the closest owner test or focused runtime path.
- For schema, protocol, generated artifact, lockfile, package, or install
  changes, identify the owning contract and use its generator or official
  recipe rather than hand-editing outputs.
- Do not stack build, test, lint, format, audit, publish, install, and runtime
  checks unless each proves a distinct required claim.
- Use broad repository or CI-parity tiers only when the user requests them,
  when preparing a PR or release, when reproducing a broad CI failure, or when
  shared executable behavior cannot be bounded by focused checks.
- Tooling success alone does not prove a runtime bug is fixed. Require the
  focused failing test or the applicable user-visible/runtime evidence.

## Tool use

- Before non-trivial tool work, send one brief preamble grouping the next
  actions.
- Use `rg` or `rg --files` first for text and file discovery. Use `fd` when its
  path predicates materially help.
- Prefer installed purpose-built tools when they fit: `jq`/`yq` for structured
  data, `delta` for diffs, `ast-grep` for syntax-aware search, `just` before
  lower-level build/test/publish commands, `cargo nextest`/`cargo shear`/
  `cargo audit`/`cargo deny` for applicable Rust workflows, `taplo`/`dprint`
  for configured formatting, and `hyperfine`/`tokei` for measurement.
- Use `apply_patch` for manual edits. Formatting and clearly mechanical bulk
  rewrites may use the owning formatter or generator.
- Never publish, install globally, stage, commit, push, or open a pull request
  unless the user asks.
- Do not re-read large files unnecessarily after a successful patch; inspect
  targeted diffs instead.

## Protected and generated material

Do not hand-edit generated files, vendored code, lockfiles, build outputs,
installed caches, or private runtime data unless the source change requires
regeneration or the owning workflow explicitly requires the update. Use the
project-context owners and protected-path list.

Never expose secrets, tokens, environment values, private resident or user data,
raw logs, or sensitive runtime state in source, docs, issues, pull requests, or
chat. Reference variable names and privacy-safe summaries when needed.
