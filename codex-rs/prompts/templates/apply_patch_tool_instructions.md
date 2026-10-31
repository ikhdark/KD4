Operation headers:

- `*** Add File: <path>`: create a file; prefix every content line with `+`.
- `*** Delete File: <path>`: delete a file; no body follows.
- `*** Update File: <path>`: patch a file with one or more `@@` hunks, or rename it without hunks.
- Put `*** Move to: <new path>` immediately after an Update header to rename it.

In a hunk, prefix unchanged context with a space, removals with `-`, and additions with `+`. Include only enough context to identify the change uniquely; one line before and after is often sufficient. Do not duplicate overlapping context between adjacent hunks. A textual `@@` anchor names an existing source line, not a containing scope: old/context lines must immediately follow it. Include every intervening line as context, or use a current `codex-range` for a distant or ambiguous edit; anchors never search later sibling bodies.

Example combining operations:

```text
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-    print("Hi")
+    print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch
```

Count-guarded literal replacement:

Use an explicit `*** Update File: <path>` for each file, with exactly one chunk `@@ codex-replace COUNT sha256:HASH`, then `-` followed by a JSON string containing the search text and `+` followed by a JSON string containing its replacement. For example, `-"old\ntext"` and `+"new\ntext"` encode multiline literals; generate these strings with `JSON.stringify` in Code Mode. HASH is the current whole-file raw UTF-8 `source_sha256`, never an excerpt or decoded UTF-16 hash. COUNT is the exact number of case-sensitive, non-overlapping occurrences in that file; 0 asserts absence. The search string must be nonempty; `+""` deletes matches. There is no regex, glob expansion, normalization or recursive replacement. Use only one Update File hunk per endpoint, including path aliases; do not combine it with line hunks. Literal replacement preserves all bytes outside matches, including mixed line endings, BOM and final-newline state; replacement bytes may intentionally change them. All file counts, revisions and candidate prerequisites are validated before any write. Use revision guards below for read-only dependencies and move destinations too.

Unique-anchor section replacement:

Use one `*** Update File: <path>` hunk with exactly one `@@ codex-anchor sha256:HASH` chunk and one `+` line containing `{"before":"unique surrounding text","after":"unique following text","replacement":"new interior"}`. Generate the object with `JSON.stringify` in Code Mode; strings may encode multiline text. HASH must be the observed whole-file raw UTF-8 `read_file` source_sha256, not an excerpt or decoded UTF-16 hash. Both anchors must be nonempty exact, case-sensitive literals, each occurring exactly once in the entire source (overlapping occurrences count). Only the bytes between them are replaced; both anchors and all outside bytes are preserved without line-ending normalization. Missing, ambiguous, reversed or overlapping anchors and stale revisions are rejected before publication. Adjacent anchors allow insertion; an empty replacement deletes the interior. Do not mix this chunk with other chunks or repeat the file through another hunk or path alias. Existing revision guards, candidate preflight and conditional publication still apply.

Rules:

- For strict JSON structured editing, use one `@@ codex-json sha256:<HASH>` chunk in an Update File hunk, with only `+` lines containing a nonempty JSON operation array, e.g. `[{"op":"replace","path":"/enabled","value":true}]`. Build the array with `JSON.stringify` in Code Mode. Supported operations are `add`, `replace`, and `remove` (`remove` omits `value`; the others require it, allowing null). HASH is the current whole-file `read_file` source_sha256. Operations execute in order in memory before publication; add inserts array elements (`-` appends) or sets object keys, replace/remove require an existing target. Empty pointer replaces the root; root removal is rejected. Untouched bytes, number/string spelling, line endings and final-newline state are preserved. Duplicate decoded keys anywhere in the source or operation values are rejected, including escaped-equivalent keys. Inputs/candidates are limited to 8 MiB and 128 nesting levels. Do not mix JSON and textual chunks. This supports strict JSON only, not JSONC or YAML.
- Prefer paths relative to the selected working directory. Absolute paths are supported, subject to the same filesystem permissions.
- For revision-bound multi-file edits, put `*** Guard File: <path>` followed by `*** Expect: sha256:<HASH>` (the whole-file `read_file` source hash) or `*** Expect: absent` before all file hunks, after any Environment ID. Guard each source, read-only dependency and move destination whose revision matters. All guards are checked before any write; a mismatch refuses the entire patch. Retained retries preserve guards; a stale guard or partially committed guarded patch requires a fresh reconciled patch, not just a hunk amendment.
- Read the relevant current function or configuration unit before patching. Submit only the intended nonempty hunk and inspect the patch receipt before continuing. If rejected, correct the request, not the safety check.
- Each source or move destination may occur only once per patch, including path aliases; put edits to a renamed file in its source Update File hunk. Order chunks from top to bottom.
- Use exact current bytes on removed lines, including indentation, trailing whitespace, and Unicode punctuation. Normalization is allowed only for unchanged context, whose original bytes are preserved.
- Add File is create-only and refuses an occupied path. Use Update File with a current codex-range source hash for a whole-file replacement. Move to replaces its destination.
- Moves write text then remove the source; they are not atomic renames and do not carry source permissions to new destinations.
- Use only this grammar. Do not include unified-diff headers such as `diff --git`, `---`, or `+++`.
- Line-hunk updates preserve the file's line endings and final-newline state; added files use LF.
- A textual hunk containing only additions appends at EOF unless its `@@` header names a context line, in which case it inserts immediately after that line.
- Include existing first lines as context to prepend. Publication is per file, not multi-file atomic. On failure or cancellation, rollback is best-effort and earlier changes may remain committed. Inspect the reported committed delta, its exactness and current files, then submit only remaining changes; do not infer all-or-nothing publication from successful preflight.
- Context, hash, and JSON conflicts are detected before any file is written: correct the patch from the current-source excerpt in the error, and re-read only when the excerpt does not cover the region. After a concurrent edit or a failure that may have modified files, re-read only the affected current sections before retrying.
- Dedicated native KD4 patch calls coordinate across processes sharing the same checkout and host temporary-directory namespace. Arbitrary editors, shell writes, older binaries, and remote providers do not participate; atomic replacement is not conditional publication against those writers.

Pass the complete patch as the tool's single multiline argument, preserving actual line breaks.
