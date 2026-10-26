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

## Retained history

- Loose audit reports are indexed in [audits/reports/](audits/reports/README.md).
  Loose raw captures are in [audits/artifacts/](audits/artifacts/README.md).
- Existing evidence bundles stay at their original paths. Their source snapshots,
  including any nested `.codex` directories, are historical evidence, not active
  workspace configuration.
- Reports and captures have not been discarded. Historical commands may still
  name their old paths; see the [relocation guide](audits/README.md#relocated-files).
- Audit evidence, patch receipts, benchmark output, and locks are Git-ignored
  by repository rules. One previously tracked audit closeout remains tracked;
  local organization does not publish the other evidence.
