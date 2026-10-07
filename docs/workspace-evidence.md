# Workspace evidence and patch execution

## File reads, patch effects, and workspace freshness

These mechanisms answer different questions:

| Mechanism | What its evidence establishes |
| --- | --- |
| `read_file` and workspace tool history | Which source a result depended on, and whether that evidence is still current. |
| `read_tool_output` | Exact content of a retained immutable snapshot, not current disk contents. |
| Turn diff | The exact text changes observed while applying patches during the turn. |

A current source read can coexist with an unavailable turn diff. Reading the
current text does not reconstruct the text that existed before an unobserved
write. Commands classified as uncertain or mutating with unknown paths use
before/after workspace evidence: an unchanged observation preserves the diff
and does not advance the mutation revision. A changed or unavailable observation
still invalidates workspace evidence. For unattributed changes, the runtime
rechecks only this turn's patch endpoints against their last observed content,
including expected absence after deletion or rename. Only mismatching or
unreadable endpoints (and their rename partners) lose exact diff content;
unrelated patch diffs survive. Known changed paths remain available for
validation attribution even if their diff content is unavailable. A test command
is not necessarily read-only; tests and build scripts can write source or snapshots.

`read_file` reads bounded UTF-8 input and retains a whole-file snapshot even when
only a selector is returned. Recovery continues through advertised selectors;
the bounded automatic subdivision plan is not a maximum recoverable file size.
Artifact retention is shared with command output, with protection
for active history. Recovery explicitly reports expired artifacts. Retention
limits are not promises that every historical snapshot remains available forever.

## Patch execution and coordination

The registered patch handler acquires the workspace operation permit before
verification and keeps it through approval, execution, mutation bookkeeping, and
diff publication. The runtime acquires a permit only when its caller did not
provide one. Local operations use a canonical repository root, falling back to
the working directory outside Git; remote patches use the selected environment's
approval scope and do not resolve remote paths on the host.

Verification rejects invalid patch endpoints and mismatching context. Execution
applies file hunks in order, stops at the first failure, and reports the committed
prefix. It is not a transaction and does not roll back earlier writes. A failed
write may leave additional effects that cannot be represented exactly.

Automatic sandbox retry is allowed only when the accumulated delta is empty and
exact. If any file changed, or a failed write leaves uncertain effects, the
runtime returns failure and recovery guidance instead of replaying the patch.
Inspect current files and construct only the remaining edits. Permission-error
text is only a heuristic for I/O failures with no observed or uncertain effects;
it does not grant approval.

Typed assignments retain task coordination, receipts, validation records, and
review gates, but no longer capture automatic change snapshots or maintain a
durable mutation ledger. Receipts and validation history do not establish current
workspace freshness; legacy epoch values are compatibility metadata, not freshness
evidence. Inspect relevant current files and coordinate overlapping work explicitly.
This does not remove turn-diff tracking or workspace tool-history invalidation.

Session approval applies to the selected environment, approval scope, and paths.
Only `ApprovedForSession` is cached. Ordinary patch approval and escalation
approval have distinct cache keys. Adding an unapproved path requires another
review of the proposed patch.

Workspace admission and execution coordination have different lifetimes. The
dispatch read/write gate orders tool calls and evidence capture. The operation
mutex serializes patch verification and writes with commands that may mutate or
validate the workspace. Validation holds the mutex so its result corresponds to
a stable revision. These gates are repository-scoped; disjoint path arguments
alone do not prove that a command or build has independent effects.

Patch metrics use the existing session telemetry system. `codex.apply_patch.attempt`
records outcome, failure kind, and whether effects are absent, exact, or uncertain.
`codex.apply_patch.verification_failed` covers direct-tool verification failures;
`codex.apply_patch.files_requested` and `codex.apply_patch.chunks_requested` describe
parsed direct-tool requests. Labels contain no paths or patch contents.
The existing `codex.tool_call` trace also records dispatch gate wait time.
