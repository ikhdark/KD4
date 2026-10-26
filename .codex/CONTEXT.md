# KD4 shared context

Last updated: 2026-10-25 (America/Chicago)
Repository HEAD when updated: `ff9393009dbd1d861d1bb5e7b8f59614b2ff1c0c`

This hash identifies the committed baseline, not uncommitted work. This brief
records decisions and historical evidence; it does not establish current source
behavior, task completion, or permission to execute checks.

## Current direction

- Maintain one small, manually curated startup brief shared between Codex and
  ordinary ChatGPT conversations. Organize by what still matters, not recency.
- Keep the three layers distinct: `AGENTS.md` owns durable guidance; this file
  holds relevant decisions and state; `audits/` holds detailed evidence. Avoid
  giant session summaries and a new context-management automation system.
- Workspace navigation: [folder guide](README.md) and [audit library](audits/README.md).
  Loose reports and captures are separated; existing run bundles keep their paths.

## Current repository state

- Uncommitted work on the baseline above removes repository-owned test-only
  files in every language: Rust suites and test-harness benchmarks, Python and
  TypeScript suites, JavaScript tests, fixtures/snapshots, dedicated helpers,
  test configuration/runners, and solely-test dependency/command wiring.
- Production examples, schema/protobuf generators, build/lint checks, and
  runtime handling of test commands in user projects remain. Dedicated repo
  test recipes are removed; SDK check recipes now lint/typecheck only.
- The removal uses source/module/manifest inspection and conservative lockfile
  reconciliation, not a build or test pass. Concurrent production edits are
  preserved. Documentation changes are also uncommitted.
- Dated audit/benchmark captures and their source snapshots remain historical
  evidence and may name removed tests or paths. Standalone obsolete task-local
  test dispatch/repair helpers are removed; recorded result data is retained.

## Significant history

### Historical recovery audit — 2026-10-25

The [final report](audits/history-recovery-2026-10-25/historical-recovery-report.txt),
pinned to `6ee609e690ef56aef917c1f6e86852cceade41f6`, records:

- Static source screening complete; zero verified recoveries and zero integrated
  source changes. This is not a clean performance verdict.
- Runtime verification blocked: the attempted owning-path runner invocations
  were refused at admission; no benchmark repetitions completed.
- Unverified candidates remain in the report, not established findings or
  restoration recommendations. No older implementation should be restored
  without an independently demonstrated benefit through its owning path while
  preserving current behavior.

These are report-stated historical results, not checks rerun for this brief.
Candidate details, exact invocations, refusal receipts and recovery requirements
stay in the linked report and its referenced evidence.

## Open work and sharing

- The recovery report requires an admitted owning-path benchmark route and an
  available permitted attempt budget before runtime follow-through. Reassess
  current authorization and source rather than treating that old blocker as
  permanent or as permission to retry.
- This is the canonical brief, not a second chat-specific memory. Ordinary
  ChatGPT cannot read the local Windows path: commit/push this file to a connected
  GitHub repository, or upload the same file when starting a conversation. No
  publication is implied by this local update.
- `audits/` is Git-ignored. A shared brief does not automatically share its linked
  evidence; upload or separately publish the relevant report when needed.
