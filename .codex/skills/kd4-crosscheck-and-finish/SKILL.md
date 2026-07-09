---
name: kd4-crosscheck-and-finish
description: Use for implementation, debugging, refactoring, integration, migration, or repo-behavior work in the C:\Users\kuh\Desktop\kd4 fork. Crosscheck the relevant AGENTS.md scope, related call paths, local-build ownership, and validation route before editing; finish only with evidence that matches this checkout.
---

# KD4 Crosscheck and Finish

## Core Rule

This checkout is the user's local fork of OpenAI Codex. A change is not
finished when the target file was edited. It is finished only when the
requested behavior is present in the normal KD4 path, reachable from
the entrypoint or workflow that owns it, and validated at the closest
practical local-build level.

Keep work fork-local unless the user explicitly asks for upstream,
product-facing, or distribution-ready changes. Preserve upstream-compatible
behavior by default, and call out changes to public CLI flags, app-server
APIs, config loading, sandbox behavior, stored sessions, rollout
compatibility, installed-binary behavior, or desktop-visible behavior.

## Crosscheck Before Editing

Root `AGENTS.md` owns task-lane selection for this checkout. Start from the
lightest safe lane there and escalate immediately when inspection shows higher
risk. The full-path discipline in this skill applies when the selected lane is
Focused code or Runtime-critical, especially for Runtime-critical work.

Before patching, inspect the smallest authoritative related-file set. Start
with the relevant `AGENTS.md` scope, then trace one hop outward from the
target file:

- runtime entrypoints, command recipes, wrappers, routers, registries, and
  consumers;
- config, schema, protocol, generated-artifact, build, package, or publish
  owners;
- focused tests, fixtures, docs, examples, and repo-local `.codex` guidance;
- platform-specific companions, stale neighboring implementations, and
  wrapper paths that can bypass the edited file.

For `codex-rs`, identify the owning crate and normal runtime path before
editing. For app-server, protocol, SDK, config-schema, generated-artifact,
or publish-path changes, identify the owning contract and generator or
recipe first.

For scripts and recipes, inspect the execution path: wrapper command,
PowerShell or POSIX assumptions, environment setup, argument forwarding,
validation hooks, and the docs that tell users how to run it.

While crosschecking, look for stale flags, unused options, wrappers that
still call older code, tests that encode old behavior, docs that describe a
different workflow, and generated outputs that must come from a recipe
instead of hand edits.

## Implement the Full Path

Update every handler, parser, registry, caller, export, config, generated
artifact, test, or document that must change for the behavior to work
through the normal path. Sweep for stale wiring after edits: unused helpers,
old call paths, missing registrations, unconsumed options, stale match arms,
schema/API mismatches, and tests that still exercise the old behavior.

Do not leave TODOs, stubs, placeholders, fake controls, inert config,
unwired exports, missing registrations, or dead branches unless the user
explicitly requested a scaffold or spike.

Do not touch patch/apply_patch guards, stale-read or preflight behavior,
approval, permission, sandbox, validation, test-gating, or execution-safety
behavior as part of unrelated work unless the user names that safety
surface.

## Respect Local State

Ignore unrelated dirty-worktree changes, untracked files, generated
artifacts, and failures outside the accepted task scope. If existing local
edits overlap the task, compare the versions, keep or produce the stronger
one, integrate compatible improvements where practical, and continue
without reverting unrelated work.

Do not hand-edit generated files, vendored code, lockfiles, Bazel metadata,
or build outputs unless the source change requires regeneration or an
owning workflow requires the update.

## Validate Locally

Use the nearest sufficient proof. Prefer focused checks over broad CI:

- Rust crate changes: run the focused crate check or test from `codex-rs`,
  using a local `just` recipe when one exists.
- App-server schema or protocol changes: run focused app-server tests and
  regenerate schema artifacts with `just write-app-server-schema` when the
  wire contract changed.
- Config schema changes: run focused config/core tests and regenerate
  `codex-rs/core/config.schema.json` with `just write-config-schema`.
- Script changes: run syntax checks and the closest script tests when
  present.
- Root maintenance changes: prefer matching root `package.json` scripts.
- Local publish changes: use `just publish-local-codex-dry-run` for path
  proof and `just publish-local-codex-final` before claiming installed local
  binary replacement.

For desktop-visible behavior, source edits do not hot-apply to the
installed app. Completion requires evidence of the runtime chain: desktop
process path, local binary hash/version, relevant app-server initialize or
model metadata, and a user-visible screenshot or equivalent runtime
evidence. If that chain was not proven, state whether visibility still
requires `just publish-local-codex-final` and a Codex Desktop restart.

If the user forbids tests or validation, skip those commands and state
exactly what remains unproven.

## Report What Was Proven

Do not describe work as fixed, done, wired, complete, working, supported, or
validated unless the implementation path was checked and the claim matches
what actually ran. If the accepted scope cannot be completed, state what is
partial, what is not wired, what blocked completion, what validation passed
or failed, and the next concrete edit required.

In the final response, summarize the material changes, why they were
required, the validation performed, and any remaining limitations.

## Interaction with Wiring Guard

If the wiring-guard plugin is active, declare wiring intent before editing
and run its check before completion claims. Use `--no-wiring-targets` only
for documentation-only or pure refactor work with no runtime call-site
wiring target. Treat an inconclusive or failing verdict as an unresolved
wiring risk, not as proof.
