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

Rules:

- Prefer paths relative to the selected working directory. Absolute paths are supported, subject to the same filesystem permissions.
- Read the relevant current function or configuration unit before patching. Submit only the intended nonempty hunk and inspect the patch receipt before continuing. If rejected, correct the request, not the safety check.
- Each source or move destination may occur only once per patch, including path aliases; put edits to a renamed file in its source Update File hunk. Order chunks from top to bottom.
- Use exact current bytes on removed lines, including indentation, trailing whitespace, and Unicode punctuation. Normalization is allowed only for unchanged context, whose original bytes are preserved.
- Add File is create-only and refuses an occupied path. Use Update File with a current codex-range source hash for a whole-file replacement. Move to replaces its destination.
- Moves write text then remove the source; they are not atomic renames and do not carry source permissions to new destinations.
- Use only this grammar. Do not include unified-diff headers such as `diff --git`, `---`, or `+++`.
- Updates preserve the file's line endings and final-newline state; added files use LF.
- A hunk containing only additions appends at EOF unless its `@@` header names a context line, in which case it inserts immediately after that line.
- Include existing first lines as context to prepend. Application is incremental: failure or cancellation may leave earlier changes committed. Inspect reported changes and current files, then submit only remaining changes.
- Context, hash, and JSON conflicts are detected before any file is written: correct the patch from the current-source excerpt in the error, and re-read only when the excerpt does not cover the region. After a concurrent edit or a failure that may have modified files, re-read only the affected current sections before retrying.
- Dedicated native KD4 patch calls coordinate across processes sharing the same checkout and host temporary-directory namespace. Arbitrary editors, shell writes, older binaries, and remote providers do not participate; atomic replacement is not conditional publication against those writers.

Pass the complete patch as the tool's single multiline argument, preserving actual line breaks.
