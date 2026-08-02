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
| Configure task-continuity hooks | [`hooks.json`](hooks.json) and [`hooks/`](hooks/) |
| Implement or change KD4 repository behavior | [`../AGENTS.md`](../AGENTS.md) |
| Follow the optional durable-artifact lifecycle | [`harness/workflow.md`](harness/workflow.md) |

Use the smallest relevant surface. Routine work does not need a harness run
directory.

## Source And State Boundary

Durable policy, hook manifests and scripts, custom agent roles, templates,
environment source, and fork-local skills are reviewable source. Generated runs,
verification output,
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

## Task Continuity

The synchronous project hooks in [`hooks.json`](hooks.json) invoke the thin
Windows PowerShell entrypoint in [`hooks/task-continuity-entry.ps1`](hooks/task-continuity-entry.ps1).
Event-specific fast scripts handle proven no-change cases; the validated
[`hooks/task-continuity.ps1`](hooks/task-continuity.ps1) path owns all changing
and error cases. Generated capsules stay ignored under
`.codex/harness/runs/task-continuity/v1/`.

Review discovery and trust through `/hooks`; never check trusted hashes into the
repository. Trust covers each normalized manifest entry, not the referenced
PowerShell file contents.

Run focused validation with:

```powershell
python -m unittest scripts.test_task_continuity_hook
python scripts/test_task_continuity_hook.py --benchmark
python scripts/test_task_continuity_hook.py --doctor
```

An actual local publish, installed-binary replacement, Desktop restart, and
Desktop-visible completion are intentionally deferred for this phase.

## Agent Lanes

Use the built-in `explorer` and `worker` for ordinary investigation and bounded
implementation. For coordinator-selected risky work, `kd4_explorer` is the
read-only contract architect and `kd4_worker` is the sole implementation owner
for the copied architect contract. `kd4_reviewer` and the read-only
`kd4_verifier` evaluate the coder assignment and final diff. The authoritative
routing and shared subagent guardrails live in the root [`AGENTS.md`](../AGENTS.md);
the durable sequence and enforcement boundary are summarized in
[`harness/workflow.md`](harness/workflow.md).
