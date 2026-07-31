# Orchestrator Template

Use this template when multi-agent work is active.

## Objective

State the shared objective and final owner.

## Coordination Pattern

Choose one:

- Pipeline: one agent's output becomes the next agent's input.
- Fanout/fanin: several agents inspect independent areas, then one owner
  integrates.
- Expert pool: agents investigate specialized surfaces such as tests, runtime,
  docs, or build tooling.
- Producer/reviewer: one agent proposes or implements, another checks.
- Supervisor: one owner tracks work, constraints, and validation evidence.

## Durable Preflight

- Root task ID:
- Starting revision and workspace fingerprint:
- Active preflight receipts checked:
- Generated-output owner:
- Validation owner:
- Cargo lanes:
- Shared/isolated strategy:

## Agent Assignments

| Agent | Assignment ID | Path/contract claims | Expected Output | Stop Condition |
| --- | --- | --- | --- | --- |
|  |  |  |  |  |

## Shared Constraints

- Follow the root and nearest scoped `AGENTS.md`.
- Do not recurse into more agents unless explicitly approved.
- Reject accidental path, named-contract, and Cargo target-lane overlap.
- Route deliberate competing implementations to separate isolated worktrees.
- Each agent reports inspected scope, findings, and validation evidence.

## Integration

- Final owner:
- Integration files:
- Versioned isolated-worktree handoffs:
- Required validation:
- Quiescence check for linked assignments, validations, gates, and claims:
- Remaining risk:
