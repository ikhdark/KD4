# Orchestrator Template

Use this template when multi-agent work is active. Follow the
[`workflow.md` multi-agent procedure](../workflow.md#optional-multi-agent-mode)
and resolve a copy of [`PREFLIGHT.json`](PREFLIGHT.json) before concurrent
writers or validation lanes start.

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
- Record path, named-contract, and Cargo target-lane overlap as advisories.
- Use isolated worktrees when separation is useful; overlap remains advisory.
- Each agent reports inspected scope, findings, and validation evidence.

## Integration

- Final owner:
- Integration files:
- Versioned isolated-worktree handoffs:
- Required validation:
- Quiescence check for linked assignments, validations, and gates:
- Remaining risk:
