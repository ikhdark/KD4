# Local Codex workspace

See the repository [AGENTS.md](../AGENTS.md) for durable instructions.
This guide is navigation, not another instruction or configuration owner.
The [root README](../README.md) indexes the active product and development guides.

## Layout

| Path | Purpose |
| --- | --- |
| [config.toml](config.toml) | Repository-local Codex configuration; kept at its runtime path. |
| [test-runners.json](test-runners.json) | Validation runner registration; kept at its runtime path. |
| [audits/](audits/README.md) | Retained historical reports/captures; new investigation reports use KDR's `audits/open-cases/`; see [case status](../../KDR/audits/README.md). |
| [fix-evidence/](fix-evidence/index.json) | Existing KD4 fix-to-receipt bindings; moved KDR evidence and shared benchmark learning are linked without copies. |
| `gitdiff/` (when present) | Runtime output path for named patch receipts from the diff-saving tool. |
| `benchmark-runs/` (when present) | Historical benchmark captures; new case output uses KDR's `benchmarks/open-cases/`; see [benchmark cases](../../KDR/benchmarks/README.md). |
| [locks/](locks/) | Tool-managed coordination files; not disposable audit clutter. |

New investigation evidence, proposals and benchmark learning use
[KDR's canonical owners](../../KDR/docs/operations/STORAGE_LAYOUT.md), not new stores here.
Both fix indexes link KDR's `learning/advisories/` projection, backed by
`learning/history.json`. KD4 keeps its separate fix registry.
Preserve existing receipt bytes and historical locations; do not mirror them.
See [evidence memory](../../KDR/docs/operations/DIAGNOSTICS.md#evidence-memory-over-time)
for selective behavioral constraints and warning-only advisories, not work-refusal gates.

## Current repository state

Repository-owned test-only files and dedicated test commands have been removed
across all languages. Build/lint checks, production examples/generators, and
mixed-purpose validation-runner registrations remain. SDK check recipes perform
lint/type checks, not tests. See [AGENTS.md](../AGENTS.md) for guidance.
Runner registration is not execution permission: the active validation mode
still applies, and retained benchmark workloads may generate and run tests.

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
