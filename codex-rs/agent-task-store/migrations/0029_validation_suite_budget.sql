-- A reservation surviving a crash is spent/uncertain, never a fresh grant.
CREATE TABLE validation_task_threads (
    thread_id TEXT PRIMARY KEY,
    task_key TEXT NOT NULL,
    grant_id TEXT NOT NULL
);
CREATE TABLE validation_suite_grants (
    task_key TEXT NOT NULL,
    grant_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('unused', 'reserved', 'started', 'uncertain')),
    attempt_id TEXT,
    acknowledgement_path TEXT,
    PRIMARY KEY (task_key, grant_id)
);
