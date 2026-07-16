# Repo-Local Codex Workspace

This directory is KD4's project-owned Codex workflow layer. It contains durable
guidance and local generated state; it is not upstream product source by
default.

## Start Here

| Need | Authoritative file |
| --- | --- |
| Edit anything under `.codex` | [`AGENTS.md`](AGENTS.md) |
| Prepare ignored files for a Codex worktree | [`environments/README.md`](environments/README.md) |
| Preserve task context, evidence, an eval, audit, or handoff | [`skills/kd4-harness/SKILL.md`](skills/kd4-harness/SKILL.md) |
| Implement or change KD4 repository behavior | [`../AGENTS.md`](../AGENTS.md) |
| Resolve ambiguous or cross-cutting source ownership | [`../SOURCEMAP.md`](../SOURCEMAP.md) |

Use the smallest relevant surface. The harness skill owns the optional artifact
format and its context-loading routes. It creates only the files a task needs
under ignored `.codex/harness/runs/`; there are no standing harness templates to
load. Routine work does not need a run directory.

## Source And State Boundary

Durable policy, environment source, and fork-local skills are reviewable source.
Generated runs, verification output, app backups, patched-app trees, and Wiring
Guard sessions are local state; the exact boundary is owned by
[`AGENTS.md`](AGENTS.md).

Project-local runtime configuration may be added as `.codex/config.toml` only
when this checkout needs an explicit setting. Its absence is intentional and is
not a missing setup step.
