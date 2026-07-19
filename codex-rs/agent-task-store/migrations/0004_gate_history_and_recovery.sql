CREATE TABLE gate_verdicts (
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id),
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    body_json TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    sealed_at TEXT NOT NULL,
    PRIMARY KEY (attempt_id, kind)
);

INSERT INTO gate_verdicts (
    attempt_id,
    assignment_id,
    kind,
    status,
    body_json,
    updated_at,
    sealed_at
)
SELECT
    CASE
        WHEN kind = '"review"'
         AND status = '"changes_requested"'
         AND EXISTS (
             SELECT 1
             FROM attempts
             WHERE assignment_id = gates.assignment_id
               AND ordinal = 1
         )
        THEN (
            SELECT attempt_id
            FROM attempts
            WHERE assignment_id = gates.assignment_id
              AND ordinal = 0
        )
        ELSE COALESCE(
            (
                SELECT attempt_id
                FROM attempts
                WHERE assignment_id = gates.assignment_id
                  AND created_at <= gates.updated_at
                ORDER BY ordinal DESC
                LIMIT 1
            ),
            (
                SELECT attempt_id
                FROM attempts
                WHERE assignment_id = gates.assignment_id
                ORDER BY ordinal ASC
                LIMIT 1
            )
        )
    END,
    assignment_id,
    kind,
    status,
    body_json,
    updated_at,
    sealed_at
FROM gates
WHERE sealed_at IS NOT NULL;

UPDATE gates
SET status = '"pending"',
    body_json = json_set(
        body_json,
        '$.status',
        'pending',
        '$.reason',
        'correction attempt requires a new review verdict',
        '$.waiver_reason',
        NULL,
        '$.updated_at',
        json((
            SELECT created_at
            FROM attempts
            WHERE assignment_id = gates.assignment_id
              AND ordinal = 1
        )),
        '$.sealed_at',
        NULL
    ),
    updated_at = (
        SELECT created_at
        FROM attempts
        WHERE assignment_id = gates.assignment_id
          AND ordinal = 1
    ),
    sealed_at = NULL
WHERE kind = '"review"'
  AND status = '"changes_requested"'
  AND EXISTS (
      SELECT 1
      FROM attempts
      WHERE assignment_id = gates.assignment_id
        AND ordinal = 1
  );

CREATE TRIGGER gate_verdicts_immutable_update
BEFORE UPDATE ON gate_verdicts
BEGIN
    SELECT RAISE(ABORT, 'gate verdicts are immutable');
END;

CREATE TRIGGER gate_verdicts_immutable_delete
BEFORE DELETE ON gate_verdicts
BEGIN
    SELECT RAISE(ABORT, 'gate verdicts are immutable');
END;

CREATE TABLE snapshot_gc_queue (
    snapshot_name TEXT PRIMARY KEY NOT NULL,
    queued_at TEXT NOT NULL
);

UPDATE write_claims
SET active = 0,
    released_at = COALESCE(released_at, created_at)
WHERE active = 1
  AND NOT EXISTS (
      SELECT 1
      FROM assignment_repositories
      WHERE assignment_repositories.assignment_id = write_claims.assignment_id
  );
