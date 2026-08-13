CREATE TABLE validation_evidence_revisions (
    attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(attempt_id),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0)
);

-- Existing rows predate every in-process rejection entry. Seed their identity
-- without hashing evidence payloads; subsequent relevant transitions advance
-- monotonically through the triggers below.
INSERT INTO validation_evidence_revisions (attempt_id, revision)
SELECT attempts.attempt_id, COUNT(validation_calls.call_id)
FROM attempts
LEFT JOIN validation_calls USING (attempt_id)
GROUP BY attempts.attempt_id;

CREATE TRIGGER validation_evidence_revision_insert
AFTER INSERT ON validation_calls
BEGIN
    INSERT INTO validation_evidence_revisions (attempt_id, revision)
    VALUES (NEW.attempt_id, 1)
    ON CONFLICT(attempt_id) DO UPDATE SET revision = revision + 1;
END;

CREATE TRIGGER validation_evidence_revision_update
AFTER UPDATE ON validation_calls
BEGIN
    INSERT INTO validation_evidence_revisions (attempt_id, revision)
    VALUES (NEW.attempt_id, 1)
    ON CONFLICT(attempt_id) DO UPDATE SET revision = revision + 1;
END;
