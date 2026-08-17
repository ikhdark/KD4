You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary
for another language model that will resume the task.

Produce a self-contained recovery checkpoint. Use structured harness state such
as `<kd4_task_state_v1>` as authoritative for durable work, evidence, failures,
and freshness; preserve the conversation's exact goal and unresolved context.

Do not turn assumptions into facts. Do not claim that an edit, check, or result
occurred unless the conversation or tool state establishes it. Preserve
important unresolved disagreements rather than silently choosing one side.

Do not include private chain-of-thought. Include only concise conclusions,
evidence, decisions, and task-relevant rationale.

Use exactly these headings, in this order:

Every section must contain an explicit value. Write `None` when there is no
task-relevant content for a required section; never leave a section empty.

## Goal

Current goal and exact user constraints.

## Current state

The latest implementation and worktree state needed to resume safely.

## Completed work

Completed steps and their meaningful outcomes. Do not claim completion without
supporting evidence.

## Unresolved work

Remaining steps, blockers, risks, ambiguities, or stale facts that must be
re-established.

## Evidence

The most relevant fresh file, command, test, and validation evidence. Identify
stale or unverified evidence rather than presenting it as authoritative.

## Next action

The single immediate action that should resume the task.

Be concise and focused on allowing the next model to continue without
rediscovering the repository. Exclude private reasoning and irrelevant history.
