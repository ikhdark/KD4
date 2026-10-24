## Repository identity and runtime boundary

- This is a private, local Codex fork. Upstream synchronization or distribution requires an explicit request.
- Treat the active repository root as the checkout location; do not hard-code it. Other sessions share this checkout.
- This repository contains the Rust CLI and app-server, not the native Windows shell. Respect `FeatureConsumer` in `codex-rs/features/src/lib.rs`: client-owned flags must not select Rust runtime behavior.
- The published fork Desktop must use `CODEX_HOME=C:\Users\kuh\Desktop\LOCAL-KD`, not the upstream home `C:\Users\kuh\.codex`.
- When explicitly asked to publish locally, use `just publish-local-codex-final`. It installs the CLI, Code Mode host, and Windows sandbox helpers together, configures Desktop, and may restart it.

## Prompt-policy ownership

- `codex-rs/protocol/src/prompts/base_instructions/default.md` is authoritative for audit/review scope, evidence and verification (including performance benchmarking), validation, and completion.
- `codex-rs/core/src/config/mod.rs` owns root/subagent roles, implementation ownership, and shared-workspace coordination.
- `codex-rs/core/src/tools/handlers/multi_agents_spec.rs` owns tool usage, assignment-field contracts, and delegation mechanics.
- Change policy in its owning source instead of copying it across layers. Other layers may keep a short reminder or reference plus layer-specific mechanics, not restate the full policy. When moving a rule, remove obsolete copies and update existing assertions to check the owner and references rather than requiring duplicated prose.

## Session analysis

- Use `C:\Users\kuh\Desktop\KDR\kdr.cmd` for Codex session audits/benchmarks; keep audit documentation in `.codex/audits`. Session-analysis scripts belong in KDR, not this repo.
- Compare matched tasks by correct outcomes, wall time, tokens, requests/tool calls, validation, retries, recovery, and compactions. Separate workflow choices from runtime effects; microbenchmarks do not establish end-to-end gains.
- Rollouts live in the fork home's `sessions` or `archived_sessions`. A `rollout_payload_artifact` stub references `rollout-payloads/<sha256>.json` in that home; resolve it before analyzing the record.


## Routine audit scope

- Keep Windows sandbox enforcement and MCP OAuth support, but exclude them from routine broad audits unless explicitly requested or implicated by a concrete issue.
- Remote control is intentionally retained; do not disable or remove it merely because a local coding rollout lacks remote traffic.

Do not run validation unless asked to.
Run compile checks at most.