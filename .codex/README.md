# Local Codex workspace

Start with [CONTEXT.md](CONTEXT.md) for the rolling brief and the repository
[AGENTS.md](../AGENTS.md) for durable instructions. This guide is navigation,
not another instruction or configuration owner.

## Layout

| Path | Purpose |
| --- | --- |
| [CONTEXT.md](CONTEXT.md) | Small, manually maintained shared context. |
| [config.toml](config.toml) | Repository-local Codex configuration; kept at its runtime path. |
| [test-runners.json](test-runners.json) | Validation runner registration; kept at its runtime path. |
| [audits/](audits/README.md) | Reports, captured artifacts, and per-run evidence bundles. |
| `gitdiff/` (when present) | Runtime output path for named patch receipts from the diff-saving tool. |
| [benchmark-runs/](benchmark-runs/) | Existing benchmark cases and their captured results. |
| [locks/](locks/) | Tool-managed coordination files; not disposable audit clutter. |

## Current repository state

Repository-owned test-only files and dedicated test commands have been removed
across all languages. Build/lint checks, production examples/generators, and
mixed-purpose validation-runner registrations remain. SDK check recipes perform
lint/type checks, not tests. See [AGENTS.md](../AGENTS.md#repository-test-state)
and [CONTEXT.md](CONTEXT.md) for guidance and the current uncommitted state.

## Retained history

- Loose audit reports are indexed in [audits/reports/](audits/reports/README.md).
  Loose raw captures are in [audits/artifacts/](audits/artifacts/README.md).
- Historical reports and benchmark captures may refer to removed tests,
  support crates, and paths; preserve those records rather than treating them as
  current runnable instructions.
- Existing evidence bundles stay at their original paths. Their source snapshots,
  including any nested `.codex` directories, are historical evidence, not active
  workspace configuration.
- Reports and captures have not been discarded. Historical commands may still
  name their old paths; see the [relocation guide](audits/README.md#relocated-files).
- Audit evidence, patch receipts, benchmark output, and locks are Git-ignored
  by repository rules. One previously tracked audit closeout remains tracked;
  local organization does not publish the other evidence.
