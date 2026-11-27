-- One immutable proof per reserved thread ID; the fingerprint identifies the
-- exact imported content whose metadata committed in the same transaction.
CREATE TABLE external_session_import_metadata_receipts (
    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    content_sha256 TEXT NOT NULL
);
