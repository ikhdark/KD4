# Repository guidance

See [the instruction architecture](codex-rs/core/instruction-architecture.md) for instruction ownership, delivery, and maintenance.

Upstream is https://github.com/openai/codex

## Session analysis

- Use `C:\Users\kuh\Desktop\KDR\kdr.cmd` for Codex session audits/benchmarks; keep audit documentation in `.codex/audits`. Session-analysis scripts belong in KDR, not this repo.
- Compare matched tasks by correct outcomes, wall time, tokens, requests/tool calls, validation, retries, recovery, and compactions. Separate workflow choices from runtime effects; microbenchmarks do not establish end-to-end gains.
- Rollouts live in the fork home's `sessions` or `archived_sessions`. A `rollout_payload_artifact` stub references `rollout-payloads/<sha256>.json` in that home; resolve it before analyzing the record.

