You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary
for another language model that will resume the task.

Produce a self-contained recovery checkpoint. Preserve the conversation's exact
goal, implementation state, observed results, failures, and unresolved context.

Do not turn assumptions into facts. Do not claim that an edit, check, or result
occurred unless the conversation or tool state establishes it. Preserve
important unresolved disagreements rather than silently choosing one side.

Do not include private chain-of-thought. Include only concise conclusions,
evidence, decisions, and task-relevant rationale.

Use exactly these headings, in this order:

Every section must contain an explicit value. Write `None` when there is no
task-relevant content for a required section; never leave a section empty.

## Goal

Current goal and exact user constraints, including prohibitions and out-of-scope work.

## Current state

First preserve verified repository context needed for the next action: the
repository root, relevant owner and symbol paths, focused build/test commands,
and affected caller or consumer relationships. Keep source snapshot or freshness
qualifications attached to these facts. Retain useful facts from ownership slices
instead of only their artifact identifiers; do not invent missing topology.

The latest implementation and worktree state needed to resume safely.
Name files already edited, partial changes, and any active ownership or handoffs.
Preserve the remaining predicted change surface: owners, files, and affected contracts.
For live commands, cells, and jobs, preserve exact identifiers, last observed
status, and supported continuation or cancellation calls. Keep unknown status
explicit; elapsed time alone does not establish completion. Retire completed handles.

## Completed work

Completed steps and their meaningful outcomes. Do not claim completion without
supporting evidence.

## Unresolved work

Remaining steps, blockers, risks, ambiguities, or stale facts that must be
re-established.
Include pending consumer updates, regeneration commands, and validation obligations.
Name preserved invariants that still need verification; retain applicable prohibitions.
Preserve ruled-out causes with their observed evidence so they are not repeated.

## Evidence

Preserve the available evidence identifier or command, scope, observed outcome,
and any known later invalidation. Mark unknown freshness explicitly; do not
invent identifiers or gather new evidence.
Keep supplied provenance labels such as direct_file_read, cached_observation,
and test_result attached to the claims they support.
Keep indispensable artifact paths and recovery references needed to resume live
work or retrieve retained output, including required selectors or cursors.

## Next action

The single immediate action that should resume the task.

Be concise and focused on allowing the next model to continue without
rediscovering the repository. Exclude private reasoning and irrelevant history.
