CREATE TABLE verification_proofs (
    target_attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempts(attempt_id),
    evaluator_attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    validation_call_ids_json TEXT NOT NULL
);

CREATE TRIGGER verification_proofs_immutable_update
BEFORE UPDATE ON verification_proofs
BEGIN
    SELECT RAISE(ABORT, 'verification proofs are immutable');
END;

CREATE TRIGGER verification_proofs_immutable_delete
BEFORE DELETE ON verification_proofs
BEGIN
    SELECT RAISE(ABORT, 'verification proofs are immutable');
END;
