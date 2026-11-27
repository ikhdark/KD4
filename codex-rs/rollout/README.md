# Rollout activity capture

Large persisted `item_completed` command items can omit `stdout` when
it is identical to `aggregated_output`. The item field
`stdout_from_aggregated_output: true` is a same-record reference, not empty
stdout. Older readers that do not know the alias still have the aggregate. Native readers restore the field; portable JSONL readers
can resolve it without another file. Live events and distinct stream views
are unchanged.

Control intermediate activity records in the fork configuration:

```toml
[features]
rollout_activity = true
```

The current [feature registry](../features/src/lib.rs) enables `rollout_activity`
by default; set it to `false` to disable capture. This is a runtime configuration
flag, not a Cargo build feature. Older binaries without the implementation cannot
enable it. It affects new events in sessions using that configuration, including
resumed sessions; it cannot recover events omitted by older runs.

Captured events use the existing JSONL `event_msg` representation:

- `reasoning_content_delta`, `reasoning_raw_content_delta`,
  `agent_reasoning_section_break`
- `plan_delta`, `agent_message_content_delta`
- `patch_apply_updated`, `turn_diff`
- `mcp_tool_call_progress`
- `exec_command_output_delta`, `terminal_interaction`
- `turn_phase_changed`, `safety_buffering`

These observations supplement, rather than replace, completed response items,
tool results, durable plan snapshots and turn timing. They do not enter model
history or change frontend rendering. Explicit non-persisting delivery and
ephemeral sessions stay non-persisting. Only model/backend content actually
emitted can be captured; this does not expose unavailable private reasoning.

New native `warning`, `error`, and `stream_error` event records carry an
optional top-level `diagnostic_origin` with `id` (an opaque UUID), `thread_id`
(the originating thread), and `timestamp` (the original recorder capture time).
An inherited record gets its usual new physical record timestamp but carries its
original origin unchanged through native forks, including bounded history copies.
An absent or malformed origin is unknown and is not inferred from message text,
record order, physical timestamp, or lineage. The event payload is unchanged.

Activity uses the existing ordered writer, timestamps, and terminal durability
barrier. There is no per-delta flush or new background queue. As with other
ordered records, an unflushed tail can be lost on process termination. Enabled
capture increases retained memory, disk traffic, and rollout size; a slow store
can add backpressure. Production latency and storage growth are not bounded by
this feature. Disable it when the additional detail is not needed.

Treat these logs as sensitive: they may contain partial source edits, command
output bytes, terminal stdin, and provider-supplied reasoning text. Output
chunks retain their protocol base64 encoding. Access and retention follow the
existing rollout directory policy.

For request diagnostics on persisted turns, inspect the `context_replay`
capture for the matching sampling request and physical attempt. This is
separate from activity events. Completed summaries, plans and tool results
persist even when intermediate activity recording is disabled.

Operational history (`RolloutRecorder::load_rollout_items` and the dependency-
version variant) excludes diagnostic `context_replay` snapshots. Recognized
current-format references are not opened or integrity-checked on this path:
they do not contribute session state or public chat history. This prevents
accumulated request snapshots from being retained while opening a task.
Ordinary history payloads, unknown formats and malformed references retain
canonical decoding/error handling. Full `for_each_rollout_item` readers and
diagnostic/export paths still hydrate and verify the saved diagnostic bodies.
Append preflight still reads, verifies and enrolls every referenced history
dependency while the writer locks are held. The append visitors do not consume
replay captures, so a recognized current-format `context_replay` reference is
only checked for a regular body file of its recorded size and enrolled for the
durable barrier; the body is not read, hashed or parsed. A missing or wrong-size
body still counts as an unresolved payload. Damage that preserves the size is
reported by the full diagnostic readers above, which still verify every body.
Unknown formats and malformed references go through the existing read, verify
and Value/RawValue decoders, and captures already parsed as Value can still be
discarded before the typed decode. Neither path changes dependency enrollment or
payload-before-rollout syncing.

A request replay capture of an immutable prepared prompt is written while the
request is dispatched; the model client joins it before delivering the
attempt's first response event, so the capture still precedes that attempt's
response items. A failed capture fails the attempt with a non-retryable error
and none of its response is consumed, although the request may already have
been sent. A capture without a prepared snapshot is still retained before
dispatch.

Before an overlapped dispatch, a small typed `pre_send_marker_v1` context-replay
row (attempt identity, transport and model only; no request body or snapshot)
is appended and synced first, so a request is never sent without at least that
durable record. If the marker cannot commit, the attempt fails with the
non-retryable `context replay pre-send marker failed` error and nothing is
sent. The full capture still follows concurrently and its failure stays fatal,
unchanged. The marker is a diagnostic `context_replay` row like the full
capture (excluded from operational history), so one marker row precedes each
overlapped full capture of a persistent session; a non-persistent session
writes none. The marker commit is attributed to the persistence local phase.
Its measured cost over 96 matched pairs of 100-turn sessions was about 3.4
percent of process CPU (median pair ratio 1.034) and a median 4 ms per turn of
wall time.

## Opt-in append writer-open timing

For a diagnostic backend launch, set `CODEX_ROLLOUT_WRITER_OPEN_TIMING=1`
and `CODEX_RUNTIME_OBSERVATIONS=1`, with
`CODEX_RUNTIME_OBSERVATIONS_DIR` pointing to an existing absolute directory.
The [runtime observations journal](../protocol/src/timing/observations.rs)
admits one `rollout_writer_open_summary` per returned append writer-open
attempt while the bounded lane is available, not one row per record. Queue
admission confirms neither append nor durability; count only rows in the journal.
A cancelled attempt need not emit a summary. A runtime-async gap marks a lost
tail when the writer can report it; writer failure or process exit can also leave
an unconfirmed tail. Rows identify the rollout by its normalized path hash, not
its path or contents.

`CODEX_RUNTIME_OBSERVATIONS=1` also captures the exact index record stream
observed during reconstruction, without requiring writer-open timing. The
`reload_rollout_observed` row uses the same bounded admission and confirms
neither an atomic snapshot nor durable storage. An absent row is not evidence
that no reconstruction occurred.

The summary reports elapsed nanoseconds for the owner, lock acquisition,
preflight and final writer materialization. Preflight sub-totals measure payload
file access and reads, SHA-256 verification, JSON parsing (including Value
construction), dependency enrollment, envelope materialization, canonical
record decode and replay-capture discard. These sub-totals exclude task
dispatch and are not a complete decomposition of preflight. Candidate and
completed-read byte counts are totals for that attempt, including repeated
references, not distinct artifacts. The projected replay count measures records
accepted by the fast parser before the canonical typed decode, not a count of
records committed to storage. `replay_size_checked` counts current-format replay
references that were size-checked instead of read (present bodies are enrolled);
their bytes are not in the completed-read total. The diagnostic does not relax validation
or change which records are read, retained or synced.

## Opt-in seven-day archive retention

For a single local home, **stop every older writer before enabling this policy**.
All future writers, including compressors, must use the upgraded build. Do not
copy or import rollout files into the home behind the publisher locks during
a sweep; stop maintenance before an out-of-band restore. Copies
outside the home that depend on its payload pool are not protected. Do not opt
in if such copies or mixed-version writers must remain supported.

Create `archive-retention.policy` directly in the selected home with the exact
UTF-8 bytes `local-home-only:7-days` followed by one LF (no BOM). Restart with
the upgraded build. Remove the marker to disable subsequent passes; removal
cannot undo a committed deletion. Other homes retain final payloads by default.

Local storage checks at startup and every six hours while running. Seven days
means time archived, not time since the last message. Preexisting archives get
a full grace period from clock migration; newly observed writer leases also get
seven days. Unarchiving prevents expiry; rearchiving restarts the clock.
The worker uses the staged native delete transaction and does not cascade into
another chat. Busy, missing, conflicting or unindexed archives and parents with
remaining children are left alone. Archive cleanup is not an unarchived-chat
storage quota.

When the home is quiet, collection scans active and archived histories (including
compressed representations), nested JSONL exports and loose home files for references before removing
unreferenced content-addressed payloads. Shared payloads remain. Live publishers,
unresolved deletion staging or an incomplete inventory defer the sweep. A writer
arriving during the scan requests that maintenance yield. An always-busy home
may need a cold start to provide a collection window.
