An earlier checkpoint summary is already present in the supplied history.
Compare against the latest effective checkpoint, including all previously
appended updates. Explicitly retire resolved blockers and superseded next actions.
Produce only a concise incremental update containing task-relevant information
that became true, changed, or remains newly unresolved after that checkpoint.
Do not repeat unchanged facts from the earlier summary unless the request names
sections to rebase. For each requested rebase, provide the complete current state,
including unchanged unresolved obligations and indispensable evidence; that body
replaces ALL prior updates for its section. For other sections, the runtime appends
your update to the existing checkpoint, so the output must stand alone as an
addendum and must not include the checkpoint preamble.
Always include `## Next action` with the single current immediate action (or
state that no action remains). This is a complete replacement, not an action log,
even when unchanged. Other sections remain additive unless explicitly rebased.
Use only the applicable standard checkpoint headings: `## Goal`,
`## Current state`, `## Completed work`, `## Unresolved work`, `## Evidence`,
and `## Next action`. Omit unchanged sections other than Next action unless rebasing them. Prefer the latest observed state
and explicitly invalidate superseded evidence.
If the user changes the goal or constraints, include `## Goal` with the complete
current goal and explicitly retire the superseded goal or constraints. The latest
explicit replacement governs subsequent work.
Record newly edited files and partial changes, changed ownership or handoffs,
pending consumer updates, regeneration and validation obligations, and newly
ruled-out causes with observed evidence. Keep supplied provenance labels and
freshness attached to the claims they support.
Update the observed diff summary when edits change; preserve unfinished hunks
and pre-existing user edits. Retain exact artifact_id values and read_tool_output
selectors or continuations for output still needed, with expiry, truncation,
and stale-workspace qualifications. Do not invent a diff that was not inspected.
In `## Current state`, preserve changed repository facts needed to continue:
owner and symbol paths, focused build/test commands, caller/consumer relationships,
and their snapshot or freshness qualifications. Retire superseded locations.
Carry changes to live command, cell, and job identifiers, their last observed
status, supported continuation or cancellation calls, and indispensable artifact
or recovery references. Retire completed handles.
Preserve unknown status; do not infer completion or gather new evidence.

Aim for at most 2,400 tokens total, with section-body targets of 250 tokens for
Goal, 350 for Current state, 250 for Completed work, 350 for Unresolved work,
500 for Evidence, and 250 for Next action (1,950 body tokens total).
These are guidance, not hard limits: preserve indispensable changed information.
An incremental update should normally be much shorter; omit unchanged sections
rather than filling their budgets. Post-hoc truncation remains a safety net.

Separate independent obligations with blank lines. Keep each obligation's
conditions, prohibitions, and evidence in the same paragraph; never separate a
qualification from the action it limits.
