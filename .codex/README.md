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
| `benchmark-runs/` (when present) | Benchmark cases and their captured results. |
| [locks/](locks/) | Tool-managed coordination files; not disposable audit clutter. |

## Current repository state

Repository-owned test-only files and dedicated test commands have been removed
across all languages. Build/lint checks, production examples/generators, and
mixed-purpose validation-runner registrations remain. SDK check recipes perform
lint/type checks, not tests. See [AGENTS.md](../AGENTS.md) for guidance and
[CONTEXT.md](CONTEXT.md) for the rolling brief.

## Retained history

- The [audit library](audits/README.md) groups reports and raw captures by task.
  At the 2026-10-26 organization, all 105 retained tasks had matching copies here
  and in `C:\Users\kuh\Desktop\KDR\audits` (see the library's organization
  report); later task folders are not guaranteed to be mirrored.
- Historical reports and benchmark captures may refer to removed tests,
  support crates, and paths; preserve those records rather than treating them as
  current runnable instructions.
- Related evidence bundles are collected inside their task folder. Their source
  snapshots, including any nested `.codex` directories, are historical evidence,
  not active workspace configuration.
- Files older than seven days were removed on 2026-10-26 by last-modified time.
  Newer reports and captures are retained. Historical commands may name old
  paths; see the [relocation guide](audits/README.md#relocated-files).
- Audit evidence, patch receipts, benchmark output, and locks are Git-ignored
  by repository rules. The formerly tracked loose closeout is included in the
  `remaining-fixes-2026-10-22` task folder; local organization publishes nothing.
