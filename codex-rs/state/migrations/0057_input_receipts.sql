CREATE TABLE input_receipts (
    thread_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    scope TEXT NOT NULL,
    client_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    payload TEXT,
    consumed INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (thread_id, kind, scope, client_id)
);
CREATE INDEX input_receipts_pending ON input_receipts(thread_id, consumed)
    WHERE payload IS NOT NULL;
CREATE TRIGGER input_receipts_delete_thread AFTER DELETE ON threads BEGIN
    DELETE FROM input_receipts WHERE thread_id = OLD.id;
END;
