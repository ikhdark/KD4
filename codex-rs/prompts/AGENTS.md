# Prompts Policy

## Ownership

This crate owns reusable model-visible prompt text and template rendering for
review, permissions, goals, compaction, and apply_patch instructions.
Treat wording changes as behavior changes.

## Editing Rules

- Preserve deterministic rendering, ordering, escaping, and line endings.
- Keep template variables explicit and covered by focused tests.
- Keep runtime policy, config resolution, and session orchestration in consumers.
- Be especially careful with permissions, approval, sandbox, goal-completion, and
  apply_patch wording because it directly steers model behavior.
- Keep `include_str!` templates inside the crate and add rendering coverage.

## Validation

- Run `cargo nextest run -p codex-prompts` or the closest focused review,
  permissions, goals, or review-exit module.
- If prompt changes affect core runtime behavior, also validate the consuming
  core or app-server path that injects the prompt.
