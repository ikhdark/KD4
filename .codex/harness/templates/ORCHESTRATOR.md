# Orchestrator Template

Use this template only when the user or active instructions explicitly ask for
subagents, delegation, or parallel agent work.

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

## Agent Assignments

| Agent | Scope | Expected Output | Stop Condition |
| --- | --- | --- | --- |
|  |  |  |  |

## Shared Constraints

- Follow the root and nearest scoped `AGENTS.md`.
- Do not recurse into more agents unless explicitly approved.
- Do not edit the same file from multiple agents without a single integrator.
- Each agent reports inspected scope, findings, and validation evidence.

## Integration

- Final owner:
- Integration files:
- Required validation:
- Remaining risk:
