-- Attempt lifecycle retires advisory intent; this is not a mutation lock table.
CREATE TABLE edit_intents (
    attempt_id TEXT NOT NULL REFERENCES attempts(attempt_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    body_json TEXT NOT NULL,
    PRIMARY KEY (attempt_id, ordinal)
);
