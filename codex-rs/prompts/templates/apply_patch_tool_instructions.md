Operation headers:

- `*** Add File: <path>`: create a file; prefix every content line with `+`.
- `*** Delete File: <path>`: delete a file; no body follows.
- `*** Update File: <path>`: patch a file with one or more `@@` hunks, or rename it without hunks.
- Put `*** Move to: <new path>` immediately after an Update header to rename it.

In a hunk, prefix unchanged context with a space, removals with `-`, and additions with `+`. Include only enough context to identify the change uniquely; one line before and after is often sufficient. Do not duplicate overlapping context between adjacent hunks. When context is not unique, name the containing class, function, or other scope after `@@`; add context or nested `@@` scopes as needed.

Example combining operations:

```text
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch
```

Rules:

- Prefer paths relative to the selected working directory. Absolute paths are supported, subject to the same filesystem permissions.
- Each source or move destination may occur only once per patch, including path aliases; put edits to a renamed file in its source Update File hunk. Order chunks from top to bottom.
- Use exact indentation on removed lines.
- Add File replaces an existing file, and Move to replaces its destination.
- Moves write text then remove the source; they are not atomic renames and do not carry source permissions to new destinations.
- Use only this grammar. Do not include unified-diff headers such as `diff --git`, `---`, or `+++`.
- Updates preserve the file's line endings and final-newline state; added files use LF.
- A hunk containing only additions appends at EOF unless its `@@` header names a context line, in which case it inserts immediately after that line.
- Include existing first lines as context to prepend. Application is incremental: failure or cancellation may leave earlier changes committed. Inspect reported changes and current files, then submit only remaining changes.
- Context, hash, and JSON conflicts are detected before any file is written: correct the patch from the current-source excerpt in the error, and re-read only when the excerpt does not cover the region. After a concurrent edit or a failure that may have modified files, re-read only the affected current sections before retrying.

Pass the complete patch as the tool's single multiline argument, preserving actual line breaks.
