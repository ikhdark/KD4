ALTER TABLE mutation_files ADD COLUMN final_snapshot_name TEXT;
ALTER TABLE mutation_files ADD COLUMN final_write_existed INTEGER
    CHECK (final_write_existed IS NULL OR final_write_existed IN (0, 1));

CREATE TABLE agent_task_bindings (
    assignment_id TEXT PRIMARY KEY NOT NULL REFERENCES assignments(assignment_id),
    attempt_id TEXT UNIQUE NOT NULL REFERENCES attempts(attempt_id),
    root_session_id TEXT NOT NULL,
    agent_path TEXT NOT NULL,
    task_name TEXT NOT NULL,
    thread_id TEXT,
    bound_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (root_session_id, agent_path)
);

CREATE INDEX agent_task_bindings_root_updated_idx
ON agent_task_bindings(root_session_id, updated_at DESC);

CREATE UNIQUE INDEX agent_task_bindings_root_thread_idx
ON agent_task_bindings(root_session_id, thread_id)
WHERE thread_id IS NOT NULL;

CREATE TRIGGER validation_calls_identity_immutable
BEFORE UPDATE OF attempt_id ON validation_calls
BEGIN
    SELECT RAISE(ABORT, 'validation call ownership is immutable');
END;

CREATE TRIGGER validation_calls_terminal_immutable
BEFORE UPDATE ON validation_calls
WHEN OLD.status <> '"running"'
BEGIN
    SELECT RAISE(ABORT, 'terminal validation calls are immutable');
END;

CREATE TRIGGER validation_calls_delete_immutable
BEFORE DELETE ON validation_calls
BEGIN
    SELECT RAISE(ABORT, 'validation calls cannot be deleted');
END;
