DROP TRIGGER assignment_repositories_immutable_update;

CREATE TRIGGER assignment_repositories_immutable_update
BEFORE UPDATE ON assignment_repositories
WHEN OLD.assignment_id <> NEW.assignment_id
  OR OLD.canonical_root <> NEW.canonical_root
  OR OLD.bound_at <> NEW.bound_at
  OR OLD.workspace_id <> NEW.workspace_id
  OR OLD.repository_id <> OLD.workspace_id
BEGIN
    SELECT RAISE(ABORT, 'assignment repository bindings are immutable');
END;
