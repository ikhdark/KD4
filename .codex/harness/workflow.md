# KD4 Harness Workflow

Use this workflow when saved context or explicitly requested delegation helps.
Follow the root [AGENTS.md](../../AGENTS.md) and nearest scoped instructions for
implementation, validation, and reporting.

## Plan, Implement, Check

1. Establish the outcome, affected owners and consumers, open questions, and
   validation route. Use [PLAN.md](templates/PLAN.md) if this needs to survive
   later turns; otherwise keep it in conversation.
2. Implement the change and keep useful decisions and progress in that same plan.
   Distinguish observed facts from assumptions and cite sources when needed.
3. Run the relevant checks. Record commands, results, and any skipped checks with
   reasons. Rerun a check when later changes affect what it tested.
4. Report what changed, what was verified, and what remains unfinished or uncertain.

Use [EVAL.md](templates/EVAL.md) when success criteria need a separate record, or
[QA_CHECKLIST.md](templates/QA_CHECKLIST.md) for a broader review. Do not copy the
same evidence into each document.

## Resume

Before interruption or compaction, update the plan with the current state and
next step. Use [HANDOFF.md](templates/HANDOFF.md) only when the plan or conversation
will not give the next turn enough context. Preserve key decisions, failed
approaches, relevant check results, and unresolved questions; omit exploration
that no longer matters.

## Optional Multi-Agent Mode

Use agents only when requested or required by applicable instructions. For work
that needs saved coordination, use
[`templates/ORCHESTRATOR.md`](templates/ORCHESTRATOR.md).

- Give each agent a bounded task, relevant instructions, dependencies, and expected
  output. Name one coordinator to integrate the work and run final validation.
- Investigators and reviewers are read-only. Workers may edit their assigned
  scope. Agents report findings or changes, supporting evidence, and open issues.
- Keep shared notes with the coordinator. Children do not stage, commit, push,
  publish, or spawn more agents unless explicitly assigned that authority.
- Sequence competing edits or use separate worktrees. Reconcile overlapping work
  before continuing affected edits or checks.
- Collect assigned results before claiming completion. If a child fails to start,
  returns a tool error, or omits its output, finish that work in the primary agent.

### Concurrent Writers and Validation

Before concurrent writers or validation lanes start, copy
[PREFLIGHT.json](templates/PREFLIGHT.json) and replace its placeholders. Resolve
`repository_root` relative to the copied manifest; claims are repository-relative.
Keep assignment details in the manifest rather than copying them into the plan.

Run `just workflow-preflight <manifest> <receipt>`. Overlap in paths, contracts,
and Cargo lanes is advisory; coordinate any competing work. Receipts expire after
one hour by default. Rerun preflight before expiry to renew a long assignment, and
run `just workflow-preflight-release <assignment-id>` when it ends.

The manifest format and registry behavior are owned by
[workflow_preflight.py](../../scripts/workflow_preflight.py).

### Bounded Review

Run one read-only review pass with at most 25 findings per reviewer. The primary
agent verifies findings and fixes them in one batch. If fixes were made, ask the
same reviewers to check those fixes and any regressions they introduced. Keep
that verification limited to the changed areas. End the review after this pass;
fix remaining issues locally and report anything unresolved. Another agent review
requires an explicit user request.

### Architect Lane

When requested, have a read-only architect describe the affected interfaces,
constraints, acceptance criteria, and validation route. Give that result to the
worker, then review the implementation against it. Resolve unclear requirements
before dependent work starts. A separate typed assignment contract is unnecessary.
