# Thread follow-up queue

The local app-server implements the Desktop queue contract without changing
version gating:

| Method | Parameters (in addition to `threadId`) | Response |
| --- | --- | --- |
| `thread/queue/list` | optional `cursor`, `limit` | `data`, `nextCursor` |
| `thread/queue/add` | `input`, optional `clientUserMessageId` | `queuedSubmission` |
| `thread/queue/update` | `queuedSubmissionId`, `input` | `queuedSubmission` |
| `thread/queue/delete` | `queuedSubmissionId` | `deleted` |
| `thread/queue/reorder` | `queuedSubmissionIds` | `{}` |
| `thread/queue/start` | `queuedSubmissionId` | `turn` |

Each queued submission contains `id`, `clientUserMessageId`, and the original
typed `input`. Mutations notify subscribed connections with
`thread/queue/changed` (`threadId`). Reordering moves the listed entries to the
front and preserves the relative order of omitted entries. Duplicate or unknown
IDs reject the whole reorder. Pagination cursors are revision-bound; restart
pagination when the queue changes.

## Execution and persistence

- Queuing never steers or interrupts the active turn. Normal completion starts
  the next entry through the existing turn-start admission path, using current
  thread settings and the submitting connection's capabilities.
- Interrupted or failed turns pause the queue. An explicit queue start resumes
  it. Rejected starts retain their input. Automatic starts require the submitting
  connection to remain subscribed.
- SQLite retains pending input across process restarts. Reopening a chat does
  not itself execute stored prompts; use queue start/send-now to resume them.
  Ephemeral and archived threads do not accept queued input.
- A per-thread OS file lock covers storage changes and core admission without
  holding a SQLite write transaction across core calls. Locks are released by
  the OS on process exit. Pending admissions retain their reserved turn ID;
  recovery checks live and persisted history before restoring or consuming the
  entry, and pauses the queue for explicit resumption.
- Retried adds with the same client message ID and unchanged input return the
  existing queued entry. Different input under that ID is rejected.
- Each thread is limited to 100 entries and a 16 MiB persisted queue payload.
  Existing turn-input validation also applies.

The schema migration is additive and thread deletion cascades to its queue.
Installing a rebuilt local binary and restarting Desktop are separate activation
steps; editing these sources does not update the running application.
