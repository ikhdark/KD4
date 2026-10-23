-- Scope wait hydration freshness to the consuming root and admitted dependencies.
-- The global revision remains for legacy callers, never as the new reuse key.
CREATE TABLE root_task_state_revision (
    root_session_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL
);
INSERT INTO root_task_state_revision
SELECT DISTINCT root_session_id, (SELECT revision FROM task_state_revision WHERE id = 1)
FROM assignments;

CREATE TRIGGER root_task_revision_assignments_insert AFTER INSERT ON assignments
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT NEW.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_assignments_update AFTER UPDATE ON assignments
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT OLD.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT NEW.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_assignments_delete AFTER DELETE ON assignments
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT OLD.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_attempts_insert AFTER INSERT ON attempts
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_attempts_update AFTER UPDATE ON attempts
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_attempts_delete AFTER DELETE ON attempts
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_receipts_insert AFTER INSERT ON receipts
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_receipts_update AFTER UPDATE ON receipts
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_receipts_delete AFTER DELETE ON receipts
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_validation_calls_insert AFTER INSERT ON validation_calls
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM attempts WHERE attempt_id = NEW.attempt_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_validation_calls_update AFTER UPDATE ON validation_calls
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM attempts WHERE attempt_id = OLD.attempt_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM attempts WHERE attempt_id = NEW.attempt_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_validation_calls_delete AFTER DELETE ON validation_calls
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM attempts WHERE attempt_id = OLD.attempt_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_gates_insert AFTER INSERT ON gates
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_gates_update AFTER UPDATE ON gates
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_gates_delete AFTER DELETE ON gates
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_workspace_actors_insert AFTER INSERT ON workspace_actors
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT NEW.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_workspace_actors_update AFTER UPDATE ON workspace_actors
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT OLD.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT NEW.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_workspace_actors_delete AFTER DELETE ON workspace_actors
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT OLD.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_agent_task_bindings_insert AFTER INSERT ON agent_task_bindings
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT NEW.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_agent_task_bindings_update AFTER UPDATE ON agent_task_bindings
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT OLD.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT NEW.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_agent_task_bindings_delete AFTER DELETE ON agent_task_bindings
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )
    UNION SELECT OLD.root_session_id) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_assignment_repositories_insert AFTER INSERT ON assignment_repositories
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_assignment_repositories_update AFTER UPDATE ON assignment_repositories
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_assignment_repositories_delete AFTER DELETE ON assignment_repositories
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_dependency_admissions_insert AFTER INSERT ON dependency_admissions
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_dependency_admissions_update AFTER UPDATE ON dependency_admissions
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_dependency_admissions_delete AFTER DELETE ON dependency_admissions
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_workspace_repositories_insert AFTER INSERT ON workspace_repositories
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM assignment_repositories WHERE workspace_id = NEW.workspace_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_workspace_repositories_update AFTER UPDATE ON workspace_repositories
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM assignment_repositories WHERE workspace_id = OLD.workspace_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM assignment_repositories WHERE workspace_id = NEW.workspace_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER root_task_revision_workspace_repositories_delete AFTER DELETE ON workspace_repositories
BEGIN
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM (SELECT root_session_id FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT assignment_id FROM assignment_repositories WHERE workspace_id = OLD.workspace_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    )) WHERE root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;


