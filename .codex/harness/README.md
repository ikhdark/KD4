# KD4 Harness

Optional notes and templates for work that needs to survive another turn.
Repository rules stay in the root [AGENTS.md](../../AGENTS.md).
[workflow.md](workflow.md) explains planning, checking, resuming, and delegation.

## Choose What Helps

For a focused task, keep the plan and results in conversation. When files would
help, start with one plan and add other templates only for a distinct need.

| Need                                              | Template                                     |
| ------------------------------------------------- | -------------------------------------------- |
| Track steps, decisions, and progress              | [PLAN.md](templates/PLAN.md)                 |
| Define and record capability or regression checks | [EVAL.md](templates/EVAL.md)                 |
| Review a broad or risky change                    | [QA_CHECKLIST.md](templates/QA_CHECKLIST.md) |
| Resume unfinished work                            | [HANDOFF.md](templates/HANDOFF.md)           |
| Coordinate explicitly requested agents            | [ORCHESTRATOR.md](templates/ORCHESTRATOR.md) |
| Check concurrent write or validation overlap      | [PREFLIGHT.json](templates/PREFLIGHT.json)   |

## Keep Task State Local

Copy selected templates into `.codex/harness/runs/<yyyy-mm-dd>-<slug>/` and fill
the copies. Remove unused sections and link to existing evidence instead of
duplicating it. Keep decisions and progress in the plan; a separate implementation
log or harness audit is unnecessary.

Run directories are local working state unless the user asks to keep them in a
patch. Keep task state out of `templates/`, and leave generated logs, screenshots,
binaries, and large transcripts out of reviewable changes unless requested.
