CREATE TABLE bugs (
    id INTEGER PRIMARY KEY,
    raw_text TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'classified')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count BETWEEN 0 AND 3),
    claim_token TEXT,
    claim_timestamp INTEGER,
    failure_category TEXT CHECK (
        failure_category IS NULL OR failure_category IN (
            'cancelled', 'provider', 'malformed_output', 'schema', 'grounding'
        )
    ),
    thread_id TEXT NOT NULL,
    cwd TEXT,
    repository_root TEXT,
    git_commit TEXT,
    summary TEXT,
    severity TEXT CHECK (
        severity IS NULL OR severity IN ('critical', 'high', 'medium', 'low')
    ),
    failure_mechanism TEXT,
    affected_components TEXT,
    stated_cause TEXT,
    required_repair TEXT,
    classifier_provider_id TEXT,
    classifier_requested_model TEXT,
    classifier_resolved_model TEXT,
    classifier_reasoning_effort TEXT,
    classifier_schema_version TEXT,
    classifier_prompt_version TEXT,
    classified_at INTEGER,
    CHECK (status = 'pending' OR summary IS NOT NULL)
);

CREATE TRIGGER bugs_submission_immutable
BEFORE UPDATE OF raw_text, thread_id, cwd, repository_root, git_commit ON bugs
BEGIN
    SELECT RAISE(ABORT, 'immutable bug submission metadata');
END;
