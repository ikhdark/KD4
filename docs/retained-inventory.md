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

Stored results describe their producer snapshots. The model requests observation
or fresh enumeration when relevant evidence changes. Read and render do not
rerun searches or claim that disk contents are still current. Artifact retention
limits still apply; an expired snapshot is reported rather than silently
recreated.
