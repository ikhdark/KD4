CREATE TABLE pending_terminal_outcomes (
    attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(attempt_id),
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    binding_json TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    status_json TEXT NOT NULL,
    event_json TEXT NOT NULL
);
CREATE INDEX pending_terminal_assignment ON pending_terminal_outcomes(assignment_id);
