DROP TRIGGER assignment_repositories_immutable_update;

ALTER TABLE assignment_repositories
ADD COLUMN workspace_id TEXT NOT NULL DEFAULT '';

UPDATE assignment_repositories
SET workspace_id = repository_id
WHERE workspace_id = '';

CREATE TRIGGER assignment_repositories_immutable_update
BEFORE UPDATE ON assignment_repositories
BEGIN
    SELECT RAISE(ABORT, 'assignment repository bindings are immutable');
END;

CREATE TABLE workspace_repositories (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    repository_id TEXT NOT NULL,
    canonical_root TEXT NOT NULL UNIQUE,
    epoch INTEGER NOT NULL DEFAULT 0 CHECK (epoch >= 0),
    updated_at TEXT NOT NULL
);

INSERT OR IGNORE INTO workspace_repositories (
    workspace_id,
    repository_id,
    canonical_root,
    epoch,
    updated_at
)
SELECT
    workspace_id,
    repository_id,
    canonical_root,
    0,
    bound_at
FROM assignment_repositories;

CREATE TABLE workspace_actors (
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    actor_id TEXT NOT NULL,
    root_session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    assignment_id TEXT REFERENCES assignments(assignment_id),
    attempt_id TEXT REFERENCES attempts(attempt_id),
    strategy TEXT NOT NULL,
    state TEXT NOT NULL,
    last_progress_at TEXT NOT NULL,
    lease_expires_at TEXT,
    PRIMARY KEY (workspace_id, actor_id)
);

CREATE TABLE contract_claims (
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    contract_name TEXT NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    created_at TEXT NOT NULL,
    released_at TEXT,
    PRIMARY KEY (workspace_id, contract_name, assignment_id)
);

CREATE TABLE workspace_paths (
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    path TEXT NOT NULL,
    content_hash TEXT,
    existed INTEGER NOT NULL CHECK (existed IN (0, 1)),
    last_epoch INTEGER NOT NULL CHECK (last_epoch >= 0),
    last_actor_id TEXT,
    attribution_confidence TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, path)
);

CREATE TABLE workspace_events (
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    epoch INTEGER NOT NULL CHECK (epoch > 0),
    actor_id TEXT,
    actor_kind TEXT NOT NULL,
    attribution_confidence TEXT NOT NULL,
    paths_json TEXT NOT NULL,
    contracts_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, epoch)
);

CREATE TABLE workspace_mutation_leases (
    lease_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    root_session_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    attempt_id TEXT REFERENCES attempts(attempt_id),
    start_epoch INTEGER NOT NULL CHECK (start_epoch >= 0),
    paths_json TEXT NOT NULL,
    contracts_json TEXT NOT NULL,
    expected_manifest_json TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    heartbeat_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT
);

CREATE TABLE validation_singleflight (
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    start_epoch INTEGER NOT NULL CHECK (start_epoch >= 0),
    fingerprint TEXT NOT NULL,
    leader_call_id TEXT NOT NULL REFERENCES validation_calls(call_id),
    state TEXT NOT NULL,
    lease_expires_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, start_epoch, fingerprint)
);

CREATE TABLE stale_recovery (
    attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(attempt_id),
    stale_events INTEGER NOT NULL DEFAULT 0 CHECK (stale_events BETWEEN 0 AND 2),
    reconciliation_call_id TEXT,
    last_reason TEXT,
    updated_at TEXT NOT NULL
);

DROP TRIGGER validation_calls_terminal_immutable;

CREATE TRIGGER validation_calls_terminal_immutable
BEFORE UPDATE ON validation_calls
WHEN OLD.status <> '"running"'
 AND NOT (
    OLD.status = '"succeeded"'
    AND NEW.status = '"superseded"'
 )
BEGIN
    SELECT RAISE(ABORT, 'terminal validation calls are immutable');
END;

CREATE INDEX workspace_actors_root_idx
ON workspace_actors(root_session_id, state, last_progress_at);

CREATE INDEX contract_claims_active_idx
ON contract_claims(workspace_id, active, contract_name);

CREATE INDEX workspace_events_created_idx
ON workspace_events(workspace_id, epoch, created_at);

CREATE INDEX workspace_mutation_leases_active_idx
ON workspace_mutation_leases(workspace_id, state, expires_at);

CREATE TRIGGER workspace_events_immutable_update
BEFORE UPDATE ON workspace_events
BEGIN
    SELECT RAISE(ABORT, 'workspace events are immutable');
END;

CREATE TRIGGER workspace_events_immutable_delete
BEFORE DELETE ON workspace_events
BEGIN
    SELECT RAISE(ABORT, 'workspace events are immutable');
END;
