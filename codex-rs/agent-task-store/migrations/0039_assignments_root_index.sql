-- Admission and unstarted-task recovery must not scan assignments from other roots.
CREATE INDEX assignments_root_session_assignment_idx
ON assignments(root_session_id, assignment_id);
