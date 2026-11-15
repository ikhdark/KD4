# Local Codex workspace

See the repository [AGENTS.md](../AGENTS.md) for durable instructions.
This guide is navigation, not another instruction or configuration owner.
The [root README](../README.md) indexes the active product and development guides.

## Layout

| Path | Purpose |
| --- | --- |
| [config.toml](config.toml) | Repository-local Codex configuration; kept at its runtime path. |
| [test-runners.json](test-runners.json) | Validation runner registration; kept at its runtime path. |
| [audits/](audits/README.md) | Retained historical KD4 reports/captures (read-only). New investigation reports are KDR audit cases; see the [report library](../../KDR/audits/README.md). |
| [fix-evidence/](fix-evidence/index.json) | Existing KD4 fix-to-receipt bindings; see the [registry guide](fix-evidence/README.md). Moved KDR evidence and shared benchmark learning are linked without copies. |
| `gitdiff/` (when present) | Runtime output path for named patch receipts from the diff-saving tool. |
| `benchmark-runs/` (when present) | Historical benchmark captures. New benchmark evidence goes to KDR; see the [storage layout](../../KDR/docs/operations/STORAGE_LAYOUT.md). |
| [locks/](locks/) | Tool-managed coordination files; not disposable audit clutter. |
| `sg/` | Unreferenced local source snapshot (`codex-rs/`, `third_party/`, 2026-11-03); not live source or configuration. |
| `v/` | Unreferenced explicit Cargo lanes-root marker and lock files (2026-11-03); not live configuration. |

New investigation evidence, proposals and benchmark learning use
[KDR's canonical owners](../../KDR/docs/operations/STORAGE_LAYOUT.md), not new stores here.
Both fix indexes link KDR's `learning/advisories/` projection, backed by
`learning/history.json`. KD4 keeps its separate fix registry.
Preserve existing receipt bytes and historical locations; do not mirror them.
See [evidence memory](../../KDR/docs/operations/DIAGNOSTICS.md#evidence-memory-over-time)
for selective behavioral constraints and warning-only advisories, not work-refusal gates.

## Current repository state

First-party test-only files and dedicated test commands have been removed
from the maintained source. This does not describe vendored dependencies, installed
environments, or frozen evidence/source snapshots. Build/lint checks, production examples/generators, and
mixed-purpose validation-runner registrations remain. SDK check recipes perform
lint/type checks, not tests; the Python recipe also synchronizes dependencies.
See [the script workflow](../scripts/README.md#other-workflows) and
[AGENTS.md](../AGENTS.md) for guidance.
Runner registration is not execution permission: the active validation mode
still applies. Inspect a retained workload before running it; one that creates
or runs tests remains forbidden under Full .5.

## Retained history

- The [audit library](audits/README.md) retains October 2026 KD4 reports and raw
  captures by task. It is not mirrored in KDR's audit library; new reports go to
  KDR.
- Historical reports and benchmark captures may refer to removed tests,
  support crates, and paths; preserve those records rather than treating them as
  current runnable instructions.
- Related evidence bundles are collected inside their task folder. Their source
  snapshots, including any nested `.codex` directories, are historical evidence,
  not active workspace configuration.
- Age-based pruning left some retained bundles incomplete. Historical commands
  may name old paths; see the [relocation guide](audits/README.md#relocated-files).
- Audit evidence, patch receipts, benchmark output and locks are Git-ignored.
