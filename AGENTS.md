# Repository guidance

See [the instruction architecture](codex-rs/core/instruction-architecture.md) for instruction/prompt ownership, delivery, and maintenance.

This is Windows only project.
This is a local project.
Do not publish or republish without permission.

Upstream is https://github.com/openai/codex

## Session context
- All implementations require targeted correctness verification and a narrow KDR benchmark to detect relevant performance regressions.
- At session start, read `.codex/CONTEXT.md`. Check its last-updated date and committed repository HEAD marker. Treat entries as historical context, never as proof of current implementation behavior or completed work; verify relevant claims against current source and evidence before relying on them.
- Keep durable instructions in `AGENTS.md`, the roughly one-page rolling brief in `.codex/CONTEXT.md`, Read linked evidence only when relevant. Organize the brief by what still matters, not merely what happened recently. Keep it consise with low verbosity. 
- When asked to record a significant session, merge durable decisions, verified outcomes, unresolved work and evidence links into the brief; keep it focused, and consise with low verbosity. Remove superseded material and omit routine activity. Refresh the date and committed HEAD marker, only keep genereated files if they are needed, distinguishing any uncommitted work from that baseline. Keep this manual rather than adding another automated management system.

## Session analysis

- Use `C:\Users\kuh\Desktop\KDR\kdr.cmd` for Codex session audits/benchmarks; keep brief and consise audit documentation in `.codex/audits`. Session-analysis scripts belong in KDR, not this repo.
- Compare matched tasks by correct outcomes, wall time, tokens, requests/tool calls, validation, retries, recovery, and compactions. Separate workflow choices from runtime effects; microbenchmarks do not establish end-to-end gains.
- Rollouts live in the fork home's `sessions`

- Implementations require a narrow benchmark for a regression and correctness check.

- At session start, review the most recent audit summaries and fix reports in `C:\Users\kuh\Desktop\kd4\.codex\audits` and `C:\Users\kuh\Desktop\KDR\audits`. Identify recently implemented, verified, reverted, and unresolved fixes, along with their original problems and measured outcomes. Before investigating or implementing a fix, check whether the issue was already addressed in those records. Verify relevant claims against current repository HEAD, uncommitted changes, and available KDR evidence. Audit reports are historical evidence, not proof of current behavior. Prioritize recent and task-relevant reports, avoid duplicate mirrored records, and read detailed evidence only when necessary.