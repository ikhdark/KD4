ALTER TABLE pending_terminal_outcomes RENAME TO pending_terminal_outcomes_v27;
DROP INDEX pending_terminal_assignment;
CREATE TABLE pending_terminal_outcomes (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    binding_json TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    status_json TEXT NOT NULL,
    event_json TEXT NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0,
    UNIQUE(attempt_id, turn_id)
);
INSERT INTO pending_terminal_outcomes (attempt_id, assignment_id, binding_json, turn_id, status_json, event_json)
SELECT attempt_id, assignment_id, binding_json, turn_id, status_json, event_json
FROM pending_terminal_outcomes_v27;
DROP TABLE pending_terminal_outcomes_v27;
CREATE INDEX pending_terminal_assignment ON pending_terminal_outcomes(assignment_id);
