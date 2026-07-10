# KD4 fork policy

This checkout is the user's local fork of
[`openai/codex`](https://github.com/openai/codex) at
`C:\Users\kuh\Desktop\kd4`. Its home repository is
[`ikhdark/KD4`](https://github.com/ikhdark/KD4).

KD4 is intended to be a modified version of Codex for the user's own
improvements, experiments, local workflows, and fork-specific tooling. Treat
work here as fork-local unless the user explicitly asks for upstream,
product-facing, or distribution-ready changes.

The standing objective is to improve, audit, and optimize this checkout while
keeping changes reviewable, local-build focused, and easy to validate.

## Instruction scope

This file applies repository-wide. Before editing, discover instruction files
with `rg --files -g AGENTS.md` and read the closest applicable file. Known
top-level scopes include `.codex/AGENTS.md`, `codex-rs/AGENTS.md`, and
`scripts/AGENTS.md`; further nested files apply only where present. Never rely on
an instruction file that is absent from the working tree.

Repo-local Codex policy and routing live under `.codex`:

- `.codex/AGENTS.md` and `.codex/README.md`: workspace policy and routing.
- `.codex/config.toml`: optional repo-local runtime configuration.
- `.codex/environments`: worktree environment setup and generated state.
- `.codex/skills`: fork-local skills and validation workflows.

Use [`SOURCEMAP.md`](SOURCEMAP.md) only when ownership is ambiguous, a task is
cross-cutting, or the runtime-to-package/publish path must be traced. For a clear
local owner, prefer the nearest scoped `AGENTS.md` and package documentation.

Keep this root policy general. Put durable package-specific rules in the nearest
nested `AGENTS.md`. README files are not loaded automatically as instructions;
promote operational editing rules into the closest `AGENTS.md` and keep READMEs
focused on usage, architecture, and background.

## Operating defaults

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

## Completion and wiring gate

Before claiming an implementation is complete, verify all of the following:

- the intended runtime or workflow path is identified and reaches the change;
- expected callers, registrations, config, and replaced or parallel paths were
  checked;
- no new or task-relevant placeholders or stubs remain in changed code or the
  intended path;
- the nearest practical validation ran, or its skip reason is explicit.

When Wiring Guard/KDWG is active, use it as the static reachability proof layer:
declare intent before implementation-shaped edits and run its check before
completion claims. Use `--no-wiring-targets` only for docs, templates, planning,
or config-only changes with no runtime call-site target.

Final implementation answers must report the completion gate (`passed`,
`partial`, or `blocked`), wiring proof, validation run, and remaining unverified
risk. Do not claim completion when the gate is partial or blocked.

## Task lanes

Choose the lightest lane that safely satisfies the request, then escalate as
soon as inspection reveals higher risk. Before non-trivial implementation work,
state the selected lane and validation intent briefly.

| Lane | Use when | Minimum evidence |
| --- | --- | --- |
| Conversation | Casual Q&A, explanation, brainstorming, or non-coding prompts | Do not inspect the repo unless the answer depends on current files or repository behavior. |
| Low-risk guidance | Documentation, instructions, comments, naming, or small non-runtime cleanup | Inspect the target and nearest `AGENTS.md`; review the focused diff and use `git diff --check` when whitespace risk exists. Escalate if source behavior, routing, config, schema, tests, generated files, or tool behavior is involved. |
| Focused code | Narrow implementation, debugging, refactoring, integration, migration, review, audit, configuration, or repo behavior with a clear owner | Inspect the owner, nearest call path, relevant config/docs, and nearest tests; run the smallest check that proves the touched behavior. |
| Runtime-critical | Safety surfaces, generated artifacts, lockfiles, shared protocol/schema contracts, desktop-visible or publish/install behavior, sandbox/approval/test-gating, dependencies, broad refactors, unclear ownership, or runtime/wiring risk | Use the full crosscheck and the owning generated-artifact, local-build, publish, or runtime proof. |

## Repository evidence guard

Use this guard only for implementation, debugging, refactoring, code review,
code audit, integration, migration, configuration, or repository-behavior tasks.
Do not apply it to casual Q&A, explanations, brainstorming, or non-coding prompts
unless the user explicitly asks for a rigorous pass.

- Do not answer from memory when the task depends on repository behavior.
- Inspect the owner files, call paths, configs, tests, and runtime entrypoints
  needed to support the claim.
- Externalize key assumptions, invariants, inspected evidence, and validation
  intent when they affect correctness.
- For implementation work, update and inspect the full path; prove stale or
  parallel code does not still win.
- For bug fixes, identify the failing path and the nearest validation that
  exercises the corrected behavior.
- For reviews and audits, state the inspected scope and avoid claims that exceed
  that scope.
- For broad claims such as "all", "every", "complete", or "repo-wide", perform
  and cite an appropriate closure sweep. If it is partial, state the exact scope
  and remaining risk.
- Do not claim a change is complete after a quick skim, partial implementation,
  compile-only check, or unverified generated output. If evidence is incomplete,
  say exactly what remains unverified.

## Validation and local-build proof

Use the nearest sufficient proof when validation is requested, required by the
task, or not explicitly waived. If the user says no tests, do not run test
commands. Do not stack build, test, lint, publish, and runtime checks unless each
proves a distinct required claim. Broad validation is reserved for shared
contracts, dependencies or workspace configuration, generated artifacts,
publish/install behavior, and desktop-visible behavior.

- Docs or guidance: focused diff review and `git diff --check` when applicable.
- Rust crates: run the focused crate check or test from `codex-rs`, preferring a
  matching local `just` recipe.
- App-server schema or protocol: run focused app-server tests and
  `just app-server-schema-check` by default. Use
  `just app-server-schema-check-force` only for intentional wire-contract
  regeneration; use `just write-app-server-schema` only when the owning workflow
  explicitly needs the raw generator.
- Config schema: run focused config/core tests and `just config-schema-check` by
  default. Use `just config-schema-check-force` only for intentional
  regeneration of `codex-rs/core/config.schema.json`; use
  `just write-config-schema` only when the owning workflow explicitly needs the
  raw generator.
- Python SDK: use focused `uv run pytest`, `uv run ruff check .`, lock checks,
  and artifact regeneration only for touched SDK surfaces.
- Scripts: run syntax checks and the closest script tests; do not hand-edit
  generated locks such as `scripts/uv.lock`.
- Root maintenance: prefer matching root `package.json` scripts.
- Local publish: use `just publish-local-codex-dry-run` for path/proof changes
  and `just publish-local-codex-final` before claiming installed replacement.

Identify the owning contract before editing protocol, app-server, SDK,
config-schema, generated-artifact, or publish-path behavior. Update generated
outputs only through the owning generator or recipe.

For a local-build claim on this checkout, prove only the applicable links in
this chain: relevant workspace/crates compile, focused tests pass, the local
`codex` binary builds, publish or dry-run paths succeed when touched, installed
replacement is correct when touched, and desktop/app-server runtime uses the
local build after rebuild, publish, and restart when desktop-visible behavior
changes.

Do not spend time on broad CI, upstream release polish, public distribution
compatibility, unrelated failures, or repo-wide cleanup unless it directly
blocks the accepted local-build goal. Tooling success alone does not prove a
runtime bug is fixed; require the focused failing test or approved final gate.

## Tool use

- Before non-trivial tool work, send one brief preamble grouping the next actions.
- Use `rg` or `rg --files` first for text and file discovery. Use `fd` when its
  path predicates materially help.
- Prefer installed purpose-built tools when they fit: `jq`/`yq` for structured
  data, `delta` for diffs, `ast-grep` for syntax-aware search, `just` before
  lower-level build/test/publish commands, `cargo nextest`/`cargo shear`/`cargo
  audit`/`cargo deny` for applicable Rust workflows, `taplo`/`dprint` for
  configured formatting, and `hyperfine`/`tokei` for measurement.
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

## Repository source map

[`SOURCEMAP.md`](SOURCEMAP.md) owns the high-level directory map, runtime
entrypoints, Rust-domain routing, build/package/publish paths, generated
contracts, and cross-cutting change routes. Keep detailed operational rules in
the closest scoped `AGENTS.md` instead of duplicating them here.

## Protected and generated paths

Do not hand-edit generated files, vendored code, lockfiles, Bazel metadata, or
build outputs unless the source change requires regeneration or an owning
workflow requires the update. In particular, do not hand-edit files under:

- `codex-rs/target`
- `node_modules`
- `codex-rs/vendor`
- `third_party`
- `codex-rs/app-server-protocol/schema`
