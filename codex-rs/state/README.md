# SQLite runtime and recovery

## Initialization

[StateRuntime](src/runtime.rs) serializes initialization and recovery across
processes with the [per-database file locks](src/runtime/recovery.rs). The lock
namespace is derived from the SQLite home, and lock paths are acquired in sorted
order. An uncontended lock is acquired on the first attempt. Contention waits
for a short async timer before retrying; cancelling admission releases any
partially acquired lock set without leaving a blocking lock request behind.

After admission, the owned task retains all locks through migration and pool
cleanup, or through quarantine and replacement initialization during recovery.
Cancelling its caller after admission does not interrupt those operations.

## Thread Git metadata and rollout compatibility

For local threads using legacy rollout history, an explicit change to the Git
SHA, branch, or origin URL commits the authoritative SQLite metadata and its
rollout compatibility record in the same transaction. An explicit clear is
authoritative even when the field is already null: rollout reconciliation and
older writers cannot refill it. An explicit replacement can set it again.
Missing values without an explicit clear remain eligible for backfill.

The [outbox](migrations/0062_thread_git_compatibility_outbox.sql) stores
publication intents in per-thread order. The [local thread store](../thread-store/src/local/update_thread_metadata.rs)
checkpoints the target append before writing and acknowledges the intent only
after confirming the exact record is durable. Startup can recover pending rows
for idle threads; an ordinary rollout append must not overtake a pending record.
An unchanged explicit update does not create a second intent for the same
change. Deleting the thread removes its pending intents.

## External session import recovery

The [import metadata receipt](migrations/0063_external_session_import_metadata_receipts.sql)
binds a reserved thread ID to the source content SHA-256 in the same SQLite
transaction as the imported metadata. When recovering an incomplete import
reservation with an existing thread, the [importer](../app-server-handlers/src/request_processors/external_agent_config_processor/sessions.rs)
requires both a matching imported history prefix and the matching receipt.
Later appends and metadata edits do not invalidate that proof. If either
check fails, recovery preserves the existing thread and reports failure rather
than replaying the metadata patch or creating a duplicate.
