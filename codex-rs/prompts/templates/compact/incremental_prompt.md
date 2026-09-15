An earlier checkpoint summary is already present in the supplied history.
Compare against the latest effective checkpoint, including all previously
appended updates. Explicitly retire resolved blockers and superseded next actions.
Produce only a concise incremental update containing task-relevant information
that became true, changed, or remains newly unresolved after that checkpoint.
Do not repeat unchanged facts from the earlier summary. The runtime will append
your update to the existing checkpoint, so the output must stand alone as an
addendum and must not include the checkpoint preamble.
Include at least one applicable heading with a non-empty body.
Use only the applicable standard checkpoint headings: `## Goal`,
`## Current state`, `## Completed work`, `## Unresolved work`, `## Evidence`,
and `## Next action`. Omit unchanged sections. Prefer the latest observed state
and explicitly invalidate superseded evidence.
If the user changes the goal or constraints, include `## Goal` with the complete
current goal and explicitly retire the superseded goal or constraints. The latest
explicit replacement governs subsequent work.
Record newly edited files and partial changes, changed ownership or handoffs,
pending consumer updates, regeneration and validation obligations, and newly
ruled-out causes with observed evidence. Keep supplied provenance labels and
freshness attached to the claims they support.
