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

- For broad KD4 audits and autonomous multi-path investigations, use the harness's hybrid ordinary-subagent/CSV-worker workflow unless the user restricts delegation. Keep small or tightly coupled work local.
- Use `C:\Users\kuh\Desktop\KDR\kdr.cmd` for Codex session audits/benchmarks; keep brief and consise audit documentation in `.codex/audits`. Session-analysis scripts belong in KDR, not this repo.
- Compare matched tasks by correct outcomes, wall time, tokens, requests/tool calls, validation, retries, recovery, and compactions. Separate workflow choices from runtime effects; microbenchmarks do not establish end-to-end gains.
- Rollouts live in the fork home's `sessions`

**Mandatory pre- and post-implementation benchmarks:** Before implementing any proposed change, including fixes recommended by audit reports, establish a narrowly scoped baseline benchmark and correctness check. After implementation, repeat the measurements under comparable conditions to verify correctness, quantify improvements, and detect regressions. Reject or revert implementations whose measured regressions outweigh their benefits. Do not classify changes as verified without sufficient evidence.

- **Review historical audits and fixes before acting:** At session start, review the most recent and task-relevant audit summaries and fix reports in `C:\Users\kuh\Desktop\kd4\.codex\audits` and `C:\Users\kuh\Desktop\KDR\audits`. Identify previously implemented, verified, reverted, and unresolved fixes, including their original problems, measured outcomes, and known regressions. Before investigating or implementing a change, check whether the issue has already been addressed. Verify historical findings against the current repository HEAD, uncommitted changes, and available KDR evidence. Treat audit reports as historical evidence, not proof of current behavior. Prioritize relevant records, avoid duplicate mirrored reports, and inspect detailed evidence only when necessary. Never reintroduce a previously regressing implementation without material new evidence demonstrating that its regressions have been resolved or are outweighed by verified benefits.

- **Mandatory diagnostic coverage:** If KDR or the KD4 harness lacks the instrumentation, telemetry, diagnostic data, or measurements required for an accurate benchmark, implement the missing capabilities before proceeding. Collect the necessary evidence, execute the benchmark, and verify correctness and regression outcomes. Never skip, defer, or mark a benchmark complete because diagnostic information is unavailable. Resolve the observability gap and produce verifiable measurements.

- Minimize benchmark output and token consumption: Execute all required benchmarks and verification, but aggressively limit unnecessary output. Suppress routine build logs, verbose diagnostics, repeated progress updates, and excessive reporting. Redirect detailed output to Git-ignored files without printing or reading it into model context unless needed for investigation. Report only essential measurements, correctness results, regressions, and actionable findings. Inspect targeted log excerpts when diagnosing failures rather than dumping entire files. Avoid generating lengthy reports or repetitive explanations unless explicitly requested. Preserve full benchmark coverage and verifiable evidence while minimizing model output tokens, context consumption, and unnecessary file generation.