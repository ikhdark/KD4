# Repo-Local Codex Workspace

This directory is KD4's project-owned Codex workflow layer. It contains durable
guidance and local generated state; it is not upstream product source by
default.

## Start Here

| Need | Authoritative file |
| --- | --- |
| Edit anything under `.codex` | [`AGENTS.md`](AGENTS.md) |
| Configure KD4 agent roles, limits, and reasoning | [`config.toml`](config.toml) and [`agents/`](agents/) |
| Prepare ignored files for a Codex worktree | [`environments/README.md`](environments/README.md) |
| Plan or preserve evidence for a durable task | [`harness/README.md`](harness/README.md) |
| Implement or change KD4 repository behavior | [`../AGENTS.md`](../AGENTS.md) |
| Follow the optional durable-artifact lifecycle | [`harness/workflow.md`](harness/workflow.md) |

Use the smallest relevant surface. Routine work does not need a harness run
directory.

## Source And State Boundary

Durable policy, custom agent roles, templates, environment source, and
fork-local skills are reviewable source. Generated runs, verification output,
app backups, patched-app trees, and specialist-tool sessions are local state;
the exact boundary is owned by [`AGENTS.md`](AGENTS.md).

Project-local runtime configuration lives in `.codex/config.toml`. Project agent
definitions live in `.codex/agents/`; every `config_file` entry must resolve to
a tracked, schema-valid role file. Roles inherit parent settings except where
their role file explicitly overrides a supported configuration key.

The `kd4_explorer`, `kd4_worker`, `kd4_reviewer`, `kd4_verifier`, and
`kd4_integrator` names are typed-capable aliases for their built-in role kinds.
The reviewer alias remains mutation-denied while retaining repository reads,
diffs, proven read-only shell commands, Repo Atlas lookups, and fetch-only
GitHub inspection.
