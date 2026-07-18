CREATE TABLE assignment_repositories (
    assignment_id TEXT PRIMARY KEY NOT NULL REFERENCES assignments(assignment_id),
    repository_id TEXT NOT NULL,
    canonical_root TEXT NOT NULL,
    bound_at TEXT NOT NULL
);

CREATE INDEX assignment_repositories_repository_idx
ON assignment_repositories(repository_id, assignment_id);

CREATE TRIGGER assignment_repositories_immutable_update
BEFORE UPDATE ON assignment_repositories
BEGIN
    SELECT RAISE(ABORT, 'assignment repository bindings are immutable');
END;

CREATE TRIGGER assignment_repositories_immutable_delete
BEFORE DELETE ON assignment_repositories
BEGIN
    SELECT RAISE(ABORT, 'assignment repository bindings are immutable');
END;
