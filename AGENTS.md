# Repository guidance

See [the instruction architecture](codex-rs/core/instruction-architecture.md) for instruction/prompt ownership, delivery, and maintenance.

This is Windows only project.
This is a local project.
Do not publish or republish without permission.

Upstream is https://github.com/openai/codex

## Session context
- Never implement a unverifed code fix.
- All implementations require at-least a narrow benchmark to make sure there are zero regressions, use KDR for metrics and benchmarks.
- At session start, read `.codex/CONTEXT.md`. Check its last-updated date and committed repository HEAD marker. Treat entries as historical context, never as proof of current implementation behavior or completed work; verify relevant claims against current source and evidence before relying on them.
- Keep durable instructions in `AGENTS.md`, the roughly one-page rolling brief in `.codex/CONTEXT.md`, Read linked evidence only when relevant. Organize the brief by what still matters, not merely what happened recently.
- When asked to record a significant session, merge durable decisions, verified outcomes, unresolved work and evidence links into the brief; remove superseded material and omit routine activity. Refresh the date and committed HEAD marker, distinguishing any uncommitted work from that baseline. Keep this manual rather than adding another automated management system.

## Session analysis

- Use `C:\Users\kuh\Desktop\KDR\kdr.cmd` for Codex session audits/benchmarks; keep audit documentation in `.codex/audits`. Session-analysis scripts belong in KDR, not this repo.
- Compare matched tasks by correct outcomes, wall time, tokens, requests/tool calls, validation, retries, recovery, and compactions. Separate workflow choices from runtime effects; microbenchmarks do not establish end-to-end gains.
- Rollouts live in the fork home's `sessions` or `archived_sessions`. A `rollout_payload_artifact` stub references `rollout-payloads/<sha256>.json` in that home; resolve it before analyzing the record.

"C:\Users\kuh\Desktop\kd4\.codex" holds records of audits and benchmark results from previous sessions.
- Put audits and benchmarks and detailed evidence in "C:\Users\kuh\Desktop\KDR\audits" and "C:\Users\kuh\Desktop\kd4\.codex\audits" in a folder with a name related to that specfic task.