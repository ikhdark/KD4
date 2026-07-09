# Prompts Policy

This file applies inside `codex-rs/prompts`. It inherits `codex-rs/AGENTS.md`.

## Ownership

This crate owns reusable model-visible prompt text and template rendering for
review, permissions, goals, compaction, realtime, and apply_patch instructions.
Treat wording changes as behavior changes.

## Editing Rules

- Preserve deterministic rendering, ordering, escaping, and line endings.
- Keep template variables explicit and covered by focused tests.
- Do not move runtime policy, config resolution, or session orchestration into
  this crate; those belong to consuming crates.
- Be especially careful with permissions, approval, sandbox, goal-completion, and
  apply_patch wording because it directly steers model behavior.
- If adding a template file used with `include_str!`, update the crate
  `BUILD.bazel` data inputs if required.

## Validation

- For template or rendering changes, run `cargo nextest run -p codex-prompts`.
- For narrow changes, run the closest focused test module, such as review,
  permissions, goals, or review-exit tests.
- If prompt changes affect core runtime behavior, also validate the consuming
  core or app-server path that injects the prompt.
