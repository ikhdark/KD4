# `.codex` workspace policy

This directory contains repo-local Codex setup, skills, and generated workflow
state for this fork. Treat it as local workflow infrastructure for
`C:\Users\kuh\Desktop\kd4`, not as upstream product source unless the user
explicitly asks for upstream-ready packaging.

The root `AGENTS.md` still applies. This file narrows the rules for files under
`.codex`.

## Durable vs generated state

Keep durable, reviewable guidance and source files separate from runtime state.

Durable files include, when present:

- `.codex/AGENTS.md`
- `.codex/environments/README.md`
- `.codex/environments/setup.py`
- `.codex/harness/README.md`
- `.codex/harness/context-modes.md`
- `.codex/harness/workflow.md`
- `.codex/harness/templates/**`
- `.codex/skills/**/SKILL.md`
- `.codex/skills/**/openai.yaml`
- `.codex/skills/**/references/**`

Generated or local runtime state includes:

- `.codex/environments/environment.toml`
- `.codex/harness/runs/**`
- `.codex/verify-local/**`
- `.codex/app-asar-backups/**`
- `.codex/app-asar-work/**`, except durable instructions explicitly kept there
- `.codex/codex-desktop-patched/**`

Do not hand-edit generated or runtime-state files unless the task is explicitly
to inspect, repair, or reset that local state. Do not treat state or cache files
as durable evidence that belongs in a patch.

## Environment setup

For `.codex/environments`, `setup.py` is the source of behavior and
`environment.toml` is generated. Keep copied paths explicit. Do not add broad
directory copies, glob synchronization, caches, vendored trees, build outputs,
or lockfiles to the environment setup unless they are required and explained.

Preserve `--dry-run` as a non-mutating preview and `--force` as intentional
overwrite behavior.

## Skills

Skills under `.codex/skills` are fork-local operating instructions. When
editing a skill:

- read the whole `SKILL.md` first;
- keep the frontmatter name and description accurate;
- avoid broad behavioral claims that the repo cannot validate;
- keep instructions actionable and scoped to this checkout;
- update neighboring metadata, such as `openai.yaml`, only when the skill
  contract actually changes.

Do not move workflow guidance into a skill when it should apply to the directory
itself; add or update the closest `AGENTS.md` instead.

## Validation

For documentation-only edits in `.codex`, inspect the rendered Markdown mentally
and use `git diff --check` when whitespace risk is non-trivial.

For `.codex/environments/setup.py`, run the focused dry-run path and any nearby
script checks that exist.

Do not run broad repo validation for `.codex` guidance changes unless the user
asks for it or the edit touches executable workflow behavior.
