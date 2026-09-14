## `apply_patch`

Use `apply_patch` to send one file-oriented patch:

```text
*** Begin Patch
[one or more file operations]
*** End Patch
```

Every operation requires one header:

- `*** Add File: <path>`: create a file; prefix every content line with `+`.
- `*** Delete File: <path>`: delete a file; no body follows.
- `*** Update File: <path>`: patch a file with one or more `@@` hunks, or rename it without hunks.
- Put `*** Move to: <new path>` immediately after an Update header to rename it.

In a hunk, prefix unchanged context with a space, removals with `-`, and additions with `+`. Normally include three context lines before and after a change; do not duplicate overlapping context between adjacent hunks. When context is not unique, name the containing class, function, or other scope after `@@`; add nested `@@` scopes if needed.

Grammar:

```text

Patch := Begin { FileOp } End

Begin := "*** Begin Patch" NEWLINE
End := "*** End Patch" NEWLINE

FileOp := AddFile | DeleteFile | UpdateFile

AddFile :=
    "*** Add File: " path NEWLINE
    { "+" line NEWLINE }

DeleteFile :=
    "*** Delete File: " path NEWLINE

UpdateFile :=
    "*** Update File: " path NEWLINE
    ( MoveTo { Hunk } | Hunk { Hunk } )

MoveTo :=
    "*** Move to: " newPath NEWLINE

Hunk :=
    "@@" [ header ] NEWLINE
    { HunkLine }
    [ "*** End of File" NEWLINE ]

HunkLine :=
    (" " | "-" | "+") text NEWLINE

```

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

Important rules:

- Use an Add, Delete, or Update header for every operation.
- Paths must be relative; never use absolute paths.
- Use only this grammar. Do not include unified-diff headers such as `diff --git`, `---`, or `+++`.
- Success proves only that the patch matched and applied, not that the result is correct or unchanged afterward.
- After stale context, a concurrent edit, a context mismatch, or a failure that may have modified files, re-read only the affected current sections before retrying. For errors known to occur before file mutation, correct the error without re-reading unchanged contents. Do not retry against stale context.
- Preserve an implementation that already satisfies the request even when it differs from an earlier plan.

Pass the complete patch as the tool's single multiline argument, preserving actual line breaks as in the example above. Use the argument format exposed by the tool interface.
