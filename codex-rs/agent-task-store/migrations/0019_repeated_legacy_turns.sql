-- no-transaction
-- Legacy message tasks can have more than one follow-up. Typed correction
-- limits remain enforced by the store; the persisted ordinal must fit u8.
-- SQLite cannot alter a CHECK constraint. Rebuild atomically with foreign keys
-- temporarily disabled outside the transaction, keeping every child reference
-- attached to the original table name. The rebuild is safe to repeat if the
-- process exits after COMMIT but before sqlx records the migration version.
PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;

CREATE TABLE attempts_expanded (
    attempt_id TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    ordinal INTEGER NOT NULL CHECK (typeof(ordinal) = 'integer' AND ordinal BETWEEN 0 AND 255),
    amendment_json TEXT,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    sealed_at TEXT,
    UNIQUE (assignment_id, ordinal)
);

INSERT INTO attempts_expanded (
    attempt_id, assignment_id, ordinal, amendment_json, state, created_at, sealed_at
)
SELECT attempt_id, assignment_id, ordinal, amendment_json, state, created_at, sealed_at
FROM attempts;

DROP TABLE attempts;
ALTER TABLE attempts_expanded RENAME TO attempts;

CREATE INDEX attempts_assignment_ordinal_idx ON attempts(assignment_id, ordinal DESC);

CREATE TRIGGER attempts_amendment_immutable
BEFORE UPDATE OF assignment_id, ordinal, amendment_json, created_at ON attempts
BEGIN
    SELECT RAISE(ABORT, 'attempt amendments are immutable');
END;

COMMIT;
PRAGMA foreign_keys = ON;
