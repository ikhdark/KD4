# Repo-Local Codex Workspace

This directory contains local Codex workflow guidance and generated state for
the `C:\Users\kuh\Desktop\kd4` fork.

Durable guidance belongs in tracked files such as `.codex/AGENTS.md`, this
README, `.codex/environments/README.md`, `.codex/harness/**`, and fork-local
skills under `.codex/skills`.

Generated runtime state belongs under directories such as `.codex/verify-local`.
Treat those files as local evidence or cache output, not as source for normal
patches.

The KD4 harness lives under `.codex/harness`. Its `README.md`,
`context-modes.md`, `workflow.md`, and `templates/**` files are durable workflow
guidance. Per-task run directories under `.codex/harness/runs/**` are local
working state unless the user asks to keep them in reviewable changes.
Use the harness for explicit harness work and for KD4 implementation tasks that
invoke Wiring Guard/KDWG, `wire-implementations`, or static wiring proof; those
tasks can use lightweight harness mode without creating a run directory.

Project-local Codex configuration may be added as `.codex/config.toml` when this
checkout needs repo-specific runtime settings. If it is absent, do not infer a
missing setup step from the filename alone.
