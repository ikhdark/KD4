-- NULL means a legacy index has not yet recovered the shared identity from history.
ALTER TABLE threads ADD COLUMN session_id TEXT;

-- A completed pre-identity index is not complete for the new nullable column.
-- Reuse bounded reconciliation rather than synthesizing an identity from ThreadId.
UPDATE backfill_state SET status = 'pending', last_watermark = NULL
WHERE status = 'complete';

-- Keep explicit deletion evidence indefinitely: missing endpoints alone are legitimate.
CREATE TABLE deleted_thread_owners (
    thread_id TEXT PRIMARY KEY NOT NULL
);

CREATE TRIGGER reject_deleted_thread_metadata
BEFORE INSERT ON threads
WHEN EXISTS (SELECT 1 FROM deleted_thread_owners WHERE thread_id = NEW.id)
BEGIN
    SELECT RAISE(ABORT, 'thread was explicitly deleted');
END;

CREATE TRIGGER reject_deleted_thread_spawn_insert
BEFORE INSERT ON thread_spawn_edges
WHEN EXISTS (SELECT 1 FROM deleted_thread_owners
             WHERE thread_id IN (NEW.parent_thread_id, NEW.child_thread_id))
BEGIN
    SELECT RAISE(ABORT, 'thread-spawn owner was explicitly deleted');
END;

CREATE TRIGGER reject_deleted_thread_spawn_update
BEFORE UPDATE ON thread_spawn_edges
WHEN EXISTS (SELECT 1 FROM deleted_thread_owners
             WHERE thread_id IN (NEW.parent_thread_id, NEW.child_thread_id))
BEGIN
    SELECT RAISE(ABORT, 'thread-spawn owner was explicitly deleted');
END;
