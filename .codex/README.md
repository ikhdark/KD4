# Repo-Local Codex Workspace

This directory is KD4's project-owned Codex workflow layer. It contains durable
guidance and local generated state; it is not upstream product source by
default.

## Start Here

| Need | Authoritative file |
| --- | --- |
| Edit anything under `.codex` | [`AGENTS.md`](AGENTS.md) |
| Configure KD4 subagent roles, limits, and reasoning | [`config.toml`](config.toml) and [`agents/`](agents/) |
| Prepare ignored files for a Codex worktree | [`environments/README.md`](environments/README.md) |
| Preserve task context, evidence, an eval, audit, or handoff | [`skills/kd4-harness/SKILL.md`](skills/kd4-harness/SKILL.md) |
| Implement or change KD4 repository behavior | [`../AGENTS.md`](../AGENTS.md) |
| Resolve ambiguous or cross-cutting source ownership | [`../SOURCEMAP.md`](../SOURCEMAP.md) |

Use the smallest relevant surface. The harness skill owns the optional artifact
format and its context-loading routes. It creates only the files a task needs
under ignored `.codex/harness/runs/`; there are no standing harness templates to
load. Routine work does not need a run directory.

## Source And State Boundary

Durable policy, agent roles, environment source, and fork-local skills are
reviewable source.
Generated runs, verification output, app backups, patched-app trees, and Wiring
Guard sessions are local state; the exact boundary is owned by
[`AGENTS.md`](AGENTS.md).

Project-local runtime configuration lives in `.codex/config.toml`. Project
subagent definitions live in `.codex/agents/` and inherit the parent model and
permission mode unless their agent file overrides a supported setting.
