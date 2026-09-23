# Task workspaces and validation snapshots

In a local unrestricted filesystem session, `workspace_transaction` exposes
`begin`, `status`, and `reconcile`. Start an editing task with `begin` before
reading source. This is an explicit workflow; existing sessions do not silently
move to a different checkout.

`begin` captures tracked files and nonignored untracked files, including current
uncommitted edits. The returned workspace has its own Git index and baseline.
The original checkout's index, commits, and refs are untouched. Built-in
`read_file`, `list_files`, `apply_patch`, `exec_command`, and `shell_command`
route structured paths into the task workspace while the transaction is active.
Shell source text is not rewritten: use relative paths or the returned task
paths. Commands containing the original checkout's absolute path are rejected.
External tools are not redirected.

Recognized validation commands through `exec_command` capture another source
snapshot before launch. They run there with unique Cargo target/build directories
and `CODEX_VALIDATION_SOURCE_REVISION`. Other build tools use the unique source
directory but must keep outputs within it. Explicit Cargo target overrides are
rejected. The receipt on launch and subsequent polls identifies the source;
completion checks its file manifest for changes and marks altered inputs invalid.
A receipt never certifies later edits or the checkout after reconciliation.

`reconcile` performs a three-way merge between the captured baseline, task files,
and current original files. Detected content conflicts publish no files and leave
the task workspace available for inspection. Independent edits merge together.
Publication uses a cooperative cross-process lock and destination rechecks;
external editors do not share that lock. A retained journal records original and
replacement bytes for recovery after an I/O failure during publication. Multi-file
publication is not an atomic filesystem transaction.

Snapshots live under `CODEX_HOME/workspace-transactions/<thread-id>` and remain
available after resume. `status` returns the active path. Snapshots exclude ignored
files and external dependencies and reject links, submodules, files over 64 MiB,
more than 100,000 files, or more than 1 GiB of source. Commands that require
excluded inputs need an explicit dependency setup inside the task workspace.

Sampling freshness handling separately retains already transmitted tool outputs
and appends invalidation notices. The earlier output remains historical evidence;
the appended notice removes its claim to describe current source. Unchanged
invalidations are not repeated within the anchored sampling continuation.

These mechanisms have correctness tests. Performance gains require matched
evaluation runs; snapshot capture and isolated cold builds can themselves add
cost. Source changes require the normal local binary rebuild and Desktop restart
before they affect Desktop sessions.
