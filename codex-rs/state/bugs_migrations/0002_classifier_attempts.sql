CREATE TABLE bug_classifier_attempts (
    attempt_id TEXT PRIMARY KEY NOT NULL,
    bug_id INTEGER NOT NULL REFERENCES bugs(id),
    ordinal INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 3),
    thread_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    receipt_json TEXT,
    UNIQUE (bug_id, ordinal)
);
