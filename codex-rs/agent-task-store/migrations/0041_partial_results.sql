-- Separate provisional evidence from sealed acceptance and terminal outcomes.
CREATE TABLE partial_results (
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    version INTEGER NOT NULL CHECK (version > 0),
    body_json TEXT NOT NULL,
    PRIMARY KEY (attempt_id, version)
);

CREATE TRIGGER partial_results_immutable_update BEFORE UPDATE ON partial_results
BEGIN
    SELECT RAISE(ABORT, 'partial results are immutable');
END;

CREATE TRIGGER partial_results_immutable_delete BEFORE DELETE ON partial_results
BEGIN
    SELECT RAISE(ABORT, 'partial results are immutable');
END;
