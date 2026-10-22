-- A capsule is published after immutable assignment admission. Only its initial
-- attachment may change the row; a published capsule and all contract fields
-- remain immutable. Keep applied migration checksums unchanged.
DROP TRIGGER assignments_immutable_update;

CREATE TRIGGER assignments_immutable_update
BEFORE UPDATE ON assignments
WHEN NEW.assignment_id IS NOT OLD.assignment_id
  OR NEW.root_session_id IS NOT OLD.root_session_id
  OR NEW.created_at IS NOT OLD.created_at
  OR json_extract(OLD.body_json, '$.task_capsule') IS NOT NULL
  OR json_type(NEW.body_json, '$.task_capsule') IS NOT 'text'
  OR json_valid(json_extract(NEW.body_json, '$.task_capsule')) IS NOT 1
  OR json_remove(OLD.body_json, '$.task_capsule')
     IS NOT json_remove(NEW.body_json, '$.task_capsule')
BEGIN
    SELECT RAISE(ABORT, 'assignments are immutable');
END;
