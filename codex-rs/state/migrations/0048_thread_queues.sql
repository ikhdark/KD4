-- The app-server owns the versioned queue payload. Keep it with thread state so
-- deleting a thread also deletes its queued prompts.
CREATE TABLE thread_queues (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
    payload TEXT NOT NULL
);
