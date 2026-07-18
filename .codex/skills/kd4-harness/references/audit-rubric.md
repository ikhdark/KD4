# Audit Rubric Reference

Use this reference when the user asks to audit, score, harden, or optimize the
KD4 harness.

## Categories

Tool Coverage:
Required local tools, commands, skills, and templates exist for the accepted
workflow. Score low when the workflow depends on unstated external tools.

Context Efficiency:
Instructions are concise, progressively disclosed, and not duplicated across
root `AGENTS.md`, `.codex/AGENTS.md`, skills, and templates.

Quality Gates:
The harness points to the nearest sufficient validation for docs, scripts, Rust
crates, schemas, publish paths, and desktop-visible work.

Memory And State:
Durable decisions are preserved in reviewable files when useful, while logs,
run state, screenshots, and generated artifacts stay local by default.

Eval Coverage:
Important harness changes have capability or regression criteria. Prefer
command and rule graders over model or manual graders when possible.

Security Guardrails:
Secrets, credentials, approval, sandbox, patch, test-gating, publish, and
execution-safety surfaces are protected from unrelated edits.

Cost And Scope Control:
Subagents, broad scans, large file reads, and broad validation are used only
when they materially improve the outcome.

Git And Review Integration:
Dirty worktree state is respected, unrelated changes are ignored, and final
answers name only actions that actually happened.

Desktop Runtime Proof:
Desktop-visible claims include publish and restart evidence, process path or
hash, and user-visible runtime proof when relevant.

## Output

Use `.codex/harness/templates/HARNESS_AUDIT.md`. Findings should lead with
evidence and top actions, not generic advice.
