-- Acknowledged payloads remain replay-conflict authority, not startup work.
CREATE INDEX pending_terminal_unacknowledged_assignment
ON pending_terminal_outcomes(assignment_id, sequence) WHERE acknowledged = 0;
CREATE INDEX pending_terminal_latest_attempt
ON pending_terminal_outcomes(attempt_id, sequence DESC);
