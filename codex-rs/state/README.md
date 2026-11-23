# SQLite runtime initialization

[StateRuntime](src/runtime.rs) serializes initialization and recovery across
processes with the [per-database file locks](src/runtime/recovery.rs). The lock
namespace is derived from the SQLite home, and lock paths are acquired in sorted
order. An uncontended lock is acquired on the first attempt. Contention waits
for a short async timer before retrying; cancelling admission releases any
partially acquired lock set without leaving a blocking lock request behind.

After admission, the owned task retains all locks through migration and pool
cleanup, or through quarantine and replacement initialization during recovery.
Cancelling its caller after admission does not interrupt those operations.
