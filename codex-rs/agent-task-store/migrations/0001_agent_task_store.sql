CREATE TABLE assignments (
    assignment_id TEXT PRIMARY KEY NOT NULL,
    root_session_id TEXT NOT NULL,
    body_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE attempts (
    attempt_id TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    ordinal INTEGER NOT NULL CHECK (ordinal IN (0, 1)),
    amendment_json TEXT,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    sealed_at TEXT,
    UNIQUE (assignment_id, ordinal)
);

CREATE TABLE receipts (
    attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(attempt_id),
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    status TEXT NOT NULL,
    body_json TEXT NOT NULL,
    sealed_at TEXT NOT NULL
);

CREATE TABLE gates (
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    body_json TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    sealed_at TEXT,
    PRIMARY KEY (assignment_id, kind)
);

CREATE TABLE write_claims (
    assignment_id TEXT PRIMARY KEY NOT NULL REFERENCES assignments(assignment_id),
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    scopes_json TEXT NOT NULL,
    supersedes_json TEXT NOT NULL,
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    created_at TEXT NOT NULL,
    released_at TEXT,
    superseded_by TEXT REFERENCES assignments(assignment_id)
);

CREATE TABLE validation_calls (
    call_id TEXT PRIMARY KEY NOT NULL,
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    body_json TEXT NOT NULL,
    status TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE TABLE wake_streams (
    root_session_id TEXT PRIMARY KEY NOT NULL,
    next_sequence INTEGER NOT NULL,
    retained_from_sequence INTEGER NOT NULL,
    latest_event_id TEXT
);

CREATE TABLE observations (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT UNIQUE NOT NULL,
    wake_event_id TEXT UNIQUE NOT NULL,
    root_session_id TEXT NOT NULL,
    wake_sequence INTEGER NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    kind TEXT NOT NULL,
    body_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (root_session_id, wake_sequence)
);

CREATE TABLE wake_events (
    root_session_id TEXT NOT NULL,
    wake_sequence INTEGER NOT NULL,
    event_id TEXT UNIQUE NOT NULL,
    assignment_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    body_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (root_session_id, wake_sequence)
);

CREATE TABLE mutation_files (
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    path TEXT NOT NULL,
    pre_write_hash TEXT,
    pre_write_existed INTEGER NOT NULL CHECK (pre_write_existed IN (0, 1)),
    final_hash TEXT,
    attribution_confidence TEXT NOT NULL,
    snapshot_name TEXT NOT NULL,
    snapshot_retained INTEGER NOT NULL CHECK (snapshot_retained IN (0, 1)),
    first_observed_at TEXT NOT NULL,
    finalized_at TEXT,
    PRIMARY KEY (attempt_id, path)
);

CREATE TABLE mutation_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    attempt_id TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (attempt_id, path) REFERENCES mutation_files(attempt_id, path)
);

CREATE INDEX attempts_assignment_ordinal_idx ON attempts(assignment_id, ordinal DESC);
CREATE INDEX observations_assignment_sequence_idx ON observations(assignment_id, sequence DESC);
CREATE INDEX observations_wake_idx ON observations(root_session_id, wake_sequence);
CREATE INDEX receipts_assignment_idx ON receipts(assignment_id);
CREATE INDEX wake_events_root_sequence_idx ON wake_events(root_session_id, wake_sequence);
CREATE INDEX mutation_events_file_idx ON mutation_events(attempt_id, path, created_at);

CREATE TRIGGER assignments_immutable_update
BEFORE UPDATE ON assignments
BEGIN
    SELECT RAISE(ABORT, 'assignments are immutable');
END;

CREATE TRIGGER assignments_immutable_delete
BEFORE DELETE ON assignments
BEGIN
    SELECT RAISE(ABORT, 'assignments are immutable');
END;

CREATE TRIGGER attempts_amendment_immutable
BEFORE UPDATE OF assignment_id, ordinal, amendment_json, created_at ON attempts
BEGIN
    SELECT RAISE(ABORT, 'attempt amendments are immutable');
END;

CREATE TRIGGER observations_append_only_update
BEFORE UPDATE ON observations
BEGIN
    SELECT RAISE(ABORT, 'observations are append-only');
END;

CREATE TRIGGER observations_append_only_delete
BEFORE DELETE ON observations
BEGIN
    SELECT RAISE(ABORT, 'observations are append-only');
END;

CREATE TRIGGER receipts_sealed_update
BEFORE UPDATE ON receipts
BEGIN
    SELECT RAISE(ABORT, 'receipts are sealed');
END;

CREATE TRIGGER receipts_sealed_delete
BEFORE DELETE ON receipts
BEGIN
    SELECT RAISE(ABORT, 'receipts are sealed');
END;
