## `apply_patch`

Use the `apply_patch` shell command to edit files.

Your patch language is a stripped-down, file-oriented diff format designed to
be easy to parse and safe to apply. You can think of it as a high-level
envelope:

```text
*** Begin Patch
[ one or more file sections ]
*** End Patch

Within that envelope, you get a sequence of file operations.

You MUST include a header to specify the action you are taking.

Each operation starts with one of three headers:

*** Add File: <path>

Create a new file. Every following line is a + line containing the initial
contents.

*** Delete File: <path>

Remove an existing file. Nothing follows.

*** Update File: <path>

Patch an existing file in place. This may optionally be followed immediately
by:

*** Move to: <new path>

Use this when you want to rename the file.

An update operation then contains one or more hunks, each introduced by @@,
optionally followed by a hunk header.

Within a hunk, each line starts with:

for unchanged context;
- for removed content;
+ for added content.

For instructions on context_before and context_after:

By default, show three lines of code immediately above and three lines
immediately below each change.
If a change is within three lines of a previous change, do not duplicate the
first change’s context_after lines in the second change’s context_before
lines.
If three lines of context are insufficient to uniquely identify the snippet
within the file, use the @@ operator to identify the class, function, or
other containing scope.

For example:

@@ class BaseClass
 [3 lines of pre-context]
-[old_code]
+[new_code]
 [3 lines of post-context]

If a code block is repeated so many times within a class or function that one
@@ statement and three lines of context still cannot identify the correct
location, use multiple @@ statements to narrow the scope:

@@ class BaseClass
@@     def method():
 [3 lines of pre-context]
-[old_code]
+[new_code]
 [3 lines of post-context]

The full grammar is:

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
    [ MoveTo ]
    { Hunk }

MoveTo :=
    "*** Move to: " newPath NEWLINE

Hunk :=
    "@@" [ header ] NEWLINE
    { HunkLine }
    [ "*** End of File" NEWLINE ]

HunkLine :=
    (" " | "-" | "+") text NEWLINE

A full patch can combine several operations:

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

Important rules:

You must include an Add, Delete, or Update header for every operation.
You must prefix every new line with + when creating a file.
File paths must always be relative. Never use absolute file paths.
Use only the supported patch grammar. Do not embed ordinary unified-diff
headers such as diff --git, ---, or +++.
A successful apply_patch invocation proves only that the supplied patch
matched the current file contents and was applied. It does not prove that the
resulting implementation is correct or that another user or agent did not
modify the file afterward.
After a failed patch, stale-context result, concurrent edit, or suspicious
mismatch, re-read the current relevant section before constructing another
patch.
Do not repeatedly retry the same patch against outdated context.
When the current implementation already satisfies the requested outcome,
preserve it instead of replacing it merely because it differs from an
earlier planned version.

Invoke apply_patch by passing the complete patch as one argument:

shell {
  "command": [
    "apply_patch",
    "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch\n"
  ]
}