-- Writer fences deliberately issue UPDATE state = state. Preserve their SQL
-- locking/finalization checks without treating unchanged rows as task mutations.
DROP TRIGGER task_revision_attempts_update;
CREATE TRIGGER task_revision_attempts_update AFTER UPDATE ON attempts
WHEN OLD.attempt_id IS NOT NEW.attempt_id
  OR OLD.assignment_id IS NOT NEW.assignment_id
  OR OLD.ordinal IS NOT NEW.ordinal
  OR OLD.amendment_json IS NOT NEW.amendment_json
  OR OLD.state IS NOT NEW.state
  OR OLD.created_at IS NOT NEW.created_at
  OR OLD.sealed_at IS NOT NEW.sealed_at
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

DROP TRIGGER root_task_revision_attempts_update;
CREATE TRIGGER root_task_revision_attempts_update AFTER UPDATE ON attempts
WHEN OLD.attempt_id IS NOT NEW.attempt_id
  OR OLD.assignment_id IS NOT NEW.assignment_id
  OR OLD.ordinal IS NOT NEW.ordinal
  OR OLD.amendment_json IS NOT NEW.amendment_json
  OR OLD.state IS NOT NEW.state
  OR OLD.created_at IS NOT NEW.created_at
  OR OLD.sealed_at IS NOT NEW.sealed_at
BEGIN
    -- Union both identities before following dependencies, so a shared root
    -- advances once even when the assignment identity did not change.
    INSERT INTO root_task_state_revision(root_session_id, revision)
    SELECT DISTINCT root_session_id, 1 FROM assignments WHERE assignment_id IN (
        WITH RECURSIVE affected(id) AS (
            SELECT OLD.assignment_id
            UNION
            SELECT NEW.assignment_id
            UNION
            SELECT admission.assignment_id FROM dependency_admissions admission
            JOIN affected ON admission.dependency_id = affected.id
        ) SELECT id FROM affected
    ) AND root_session_id IS NOT NULL
    ON CONFLICT(root_session_id) DO UPDATE SET revision = revision + 1;
END;
