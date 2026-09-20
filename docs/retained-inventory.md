# Retained inventories

The inventory tool retains repository inventory records. It does not decide
what belongs in a task, infer classifications, or introduce domain-specific
orchestration instructions. On search-capable models its schema is deferred;
existing tool search and exact-name activation expose it when needed.

The model supplies a scope object and a profile with categories,
classifications, and required_categories. Those values, plus the selected
environment and working directory, define an immutable scope identity. A
different definition requires a new inventory.

Each record contains scope, category, candidate, classification, evidence,
status, unresolved_reason, and provenance. The candidate contains an exact
identifier, existence, tracking state, and an optional source revision.
Classification labels belong to the profile; lifecycle statuses belong to the
tool.

## Operations

- **create** freezes the selected scope/profile and returns an inventory ID.
- **observe** inspects caller-selected paths through environment filesystem
  permissions. It records existence, content hashes, and local Git tracking.
  Remote or unavailable Git metadata is explicitly unknown. Files above the
  bounded hash-read limit have no revision; this is not proof of unchanged
  contents.
- **import** reads an exact JSON enumeration from an existing producer artifact,
  supporting other search strategies without transcribing their identifiers
  through model prose.
- **classify** attaches a profile label or unresolved reason to an existing
  record, with exact artifact/JSON-pointer or artifact/line-range evidence.
  References are validated and hashed; semantic relevance remains the model's
  responsibility.
- **read** returns bounded pages of candidate facts and record references, with
  counts, an explicit continuation, and unresolved required categories.
- **render** writes sorted, deduplicated identifiers and their derived count to
  an exact artifact/file. The document includes the scope, profile,
  classification filter, and unresolved work. The model can link this document
  and explain it without reconstructing the identifier list.

The rendered file is the authoritative final enumeration. Deliver its returned
`rendered_path` as a link when exact identifiers are required. The count and
identifiers come from the same filtered records. This protects the artifact;
it does not validate arbitrary filenames subsequently written in assistant
prose. The real-file regression exercises observation, classification,
rejection of an unsupported identifier, and registry-dispatched rendering,
then checks the final file's identifiers and count.

Always use the returned inventory_id. Updates create immutable snapshots.
Concurrent updates from one ID create explicit branches; they do not overwrite
one another or merge different definitions implicitly. Reads of an old ID read
that old snapshot.

## Enumeration and refresh

The model chooses a search strategy and obtains paths, then passes them to
observe. The tool does not recursively scan the repository without that
selection. Alternatively, import accepts an artifact containing:

~~~json
{
  "scope_id": "the value returned by create",
  "category": "a profile category",
  "complete": true,
  "unresolved_reason": null,
  "candidates": [
    {
      "id": "an exact producer identifier",
      "exists": true,
      "tracking": "tracked",
      "revision": "a producer-observed content hash"
    }
  ]
}
~~~

Existence and revision may be explicitly null; tracking is tracked, untracked,
or unknown. Import preserves the producer's observations and provenance; it
does not certify their current filesystem state.

Complete means the input is the **whole selected category enumeration**, not the
last page of a partial batch. An incomplete batch requires a reason and merges
without removing previous candidates. A complete enumeration reconciles the
set and records additions, changes, and removals. Removal means absence from the
enumeration, not proven file deletion.

Identical duplicates collapse; conflicting duplicate identifiers are rejected.
Unchanged observations preserve prior classifications. Changed observations
retain the prior decision/evidence but mark it stale until reviewed. An
unchanged import reuses the prior inventory ID and reports that reuse. A fresh
observation with an unknown revision marks a prior classification stale and
reports it as unverified, rather than claiming the source changed or stayed
unchanged. Detailed changes are retained in an artifact rather than repeated
inline.

Required category coverage and unresolved/stale records are reported separately.
Rendering a partial inventory never promotes it to complete. Identifiers in
several categories count once in the rendered union; the classification filter
remains part of the rendered document.

## Storage and boundaries

Snapshots and evidence use the existing thread-confined tool-output artifact
store and read_tool_output recovery. There is no new database or prompt-specific
persistence mechanism. Enumeration sources must be complete, integrity-checked
artifacts within the 4 MiB transformation limit. Evidence may select a bounded
exact range from a larger artifact. Evidence is validated when attached; its
references remain subject to artifact retention. Attaching expired, cross-thread,
incomplete, or invalid evidence causes an explicit error instead of an empty or
reconstructed result.

Render results use the exact export as their canonical tool output, even when
the receipt fits inline.
The existing history projection and compaction recovery pins therefore identify
the exported identifiers and scope directly, rather than only a receipt linking
to a separate file. Recovery uses `read_tool_output` with `/identifiers` and
`/summary` selectors; it requires no source enumeration.

Stored results describe their producer snapshots. The model requests observation
or fresh enumeration when relevant evidence changes. Read and render do not
rerun searches or claim that disk contents are still current. Artifact retention
limits still apply; an expired snapshot is reported rather than silently
recreated.

## File reads, patch effects, and workspace freshness

These mechanisms answer different questions:

| Mechanism | What its evidence establishes |
| --- | --- |
| `read_file` and workspace tool history | Which source a result depended on, and whether that evidence is still current. |
| `read_tool_output` | Exact content of a retained immutable snapshot, not current disk contents. |
| `retained_inventory` | The selected enumeration, classifications, and their producer evidence. Refresh is explicit. |
| Turn diff | The exact text changes observed while applying patches during the turn. |
| Known Delta | Reusable output for an immutable Git object, keyed by object and authorization identity. |

A current source read can coexist with an unavailable turn diff. Reading the
current text does not reconstruct the text that existed before an unobserved
write. Unknown command effects therefore invalidate the exact turn diff even
when later reads establish current file contents. Commands classified as
uncertain, including arbitrary test recipes, use before/after workspace evidence:
unchanged observations preserve the diff, while changed or unavailable evidence
invalidates it. A test command is not necessarily read-only; tests and build
scripts can write source or snapshots.

`read_file` reads bounded UTF-8 input and retains a whole-file snapshot even when
only a selector is returned. Recovery continues through advertised selectors;
the bounded automatic subdivision plan is not a maximum recoverable file size.
Artifact retention is shared with command output and inventories, with protection
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

Typed assignments record durable mutation evidence before writes and finalize it
afterward. Finalization failure leaves filesystem changes committed and produces
a model-visible warning that the ledger may be incomplete. Terminal cancellation,
declined retry approval, and hook failure also finalize pending evidence.

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
parsed direct-tool requests. `codex.apply_patch.evidence_finalization_failed`
counts failed ledger finalizations. Labels contain no paths or patch contents.
The existing `codex.tool_call` trace also records dispatch gate wait time.
