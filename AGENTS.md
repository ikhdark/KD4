# Repository guidance

See [the instruction architecture](codex-rs/core/instruction-architecture.md) for instruction/prompt ownership, delivery, and maintenance.

This is Windows only project.
This is a local project.

Upstream is https://github.com/openai/codex

## Repository test state

- Repository-owned test-only files have been removed across all languages, including suites, fixtures, snapshots, test-only helpers/configuration, and dedicated runners. Do not recreate them as incidental maintenance or validation.
- Production examples, schema/protobuf generators, build/lint tooling, and runtime support for testing user projects are not repository tests. Classify files by their consumers and purpose, not names such as `spec`, `snapshot`, or `fixture`.
- Historical audit captures and source snapshots are evidence, not active test suites. Preserve their recorded provenance; do not treat old test commands or paths as current instructions.
- Inspect current manifests before naming validation targets. Follow the active validation and test-creation policies; documentation and remaining mixed-purpose runner registrations do not authorize execution.

## Session context

- At session start, read `.codex/CONTEXT.md`. Check its last-updated date and committed repository HEAD marker. Treat entries as historical context, never as proof of current implementation behavior or completed work; verify relevant claims against current source and evidence before relying on them.
- Keep durable instructions in `AGENTS.md`, the roughly one-page rolling brief in `.codex/CONTEXT.md`, and detailed evidence in `.codex/audits/`. Read linked evidence only when relevant. Organize the brief by what still matters, not merely what happened recently.
- When asked to record a significant session, merge durable decisions, verified outcomes, unresolved work and evidence links into the brief; remove superseded material and omit routine activity. Refresh the date and committed HEAD marker, distinguishing any uncommitted work from that baseline. Keep this manual rather than adding another automated management system.

## Session analysis

- Use `C:\Users\kuh\Desktop\KDR\kdr.cmd` for Codex session audits/benchmarks; keep audit documentation in `.codex/audits`. Session-analysis scripts belong in KDR, not this repo.
- Compare matched tasks by correct outcomes, wall time, tokens, requests/tool calls, validation, retries, recovery, and compactions. Separate workflow choices from runtime effects; microbenchmarks do not establish end-to-end gains.
- Rollouts live in the fork home's `sessions` or `archived_sessions`. A `rollout_payload_artifact` stub references `rollout-payloads/<sha256>.json` in that home; resolve it before analyzing the record.

