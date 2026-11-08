Edit one or many files for one coherent change: `await tools.apply_patch(patchText)`, with the complete patch as the single multiline argument, not JSON.

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

- Add File is create-only; prefix every line with `+`. Update File takes `@@` hunks: unchanged context with a space, removals `-`, additions `+`. Include just enough context to identify the change, often one line each side; old/context lines must immediately follow a textual `@@` anchor line. Derive removed lines from the actual current read, never a remembered or inferred preimage. A hunk with only additions appends at EOF unless its `@@` names a context line.
- Each file appears once per patch, with chunks in file order. Use only this grammar, no unified-diff headers. Publication is per file, not atomic: on failure inspect the receipt and submit only the remaining changes.
- For HASH below, use `source_sha256` from a read or `write_receipt.sha256` from a patch: prints show their first 12 characters, and the full digest also works. Replace lines already read without retyping them: `@@ codex-range START:END sha256:HASH` then `+` lines (a whole-file replacement too; deletions use `-` hunks). Literal replace: `@@ codex-replace COUNT sha256:HASH` then `-"old"` and `+"new"` as JSON strings, COUNT being the exact occurrence count. JSON edit: `@@ codex-json sha256:HASH` then `+[{"op":"replace","path":"/a","value":1}]`. A stale hash is rejected before any write.
- Result: {success, operation_id, outcome, text, changes, changes_exact, environment_id, diagnostics, tool_failure?, retry?}; outcome is committed/unchanged/failed/unknown, independently of success. A rejected patch returns success:false instead of throwing, so check success before steps that need the edit; if changes_exact is false, read the affected files before retrying. Correct a rejected patch from the current-source excerpt in its error, and never bypass the check.
- Guard files for read-only dependencies (hunk context already protects the lines you edit), anchored section replacement, retries and Operation IDs, move rules and full result semantics: `resolve_tool("apply_patch").description` and the sections it lists.
