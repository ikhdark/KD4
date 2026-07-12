# Repo-Local Codex Workspace

This directory is KD4's project-owned Codex workflow layer. It contains durable
guidance and local generated state; it is not upstream product source by
default.

## Start Here

| Need | Authoritative file |
| --- | --- |
| Edit anything under `.codex` | [`AGENTS.md`](AGENTS.md) |
| Prepare ignored files for a Codex worktree | [`environments/README.md`](environments/README.md) |
| Plan or preserve evidence for a durable task | [`harness/README.md`](harness/README.md) |
| Implement or change KD4 repository behavior | [`../AGENTS.md`](../AGENTS.md) |
| Follow the optional durable-artifact lifecycle | [`harness/workflow.md`](harness/workflow.md) |
| Create, audit, or run durable harness artifacts | [`skills/kd4-harness/SKILL.md`](skills/kd4-harness/SKILL.md) |

Use the smallest relevant surface. Routine work does not need a harness run
directory, and the harness skill should load only when durable workflow state or
harness maintenance is actually requested.

## Source And State Boundary

Durable policy, templates, environment source, and fork-local skills are
reviewable source. Generated runs, verification output, app backups, patched-app
trees, and Wiring Guard sessions are local state; the exact boundary is owned by
[`AGENTS.md`](AGENTS.md).

Project-local runtime configuration may be added as `.codex/config.toml` only
when this checkout needs an explicit setting. Its absence is intentional and is
not a missing setup step.
