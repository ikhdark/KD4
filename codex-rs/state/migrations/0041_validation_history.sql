CREATE TABLE validation_history_aggregates (
    scope_kind INTEGER NOT NULL,
    repository_id TEXT NOT NULL,
    fingerprint_id TEXT NOT NULL,
    operation INTEGER NOT NULL,
    ecosystem INTEGER NOT NULL,
    breadth INTEGER NOT NULL,
    model_version INTEGER NOT NULL,
    key_version INTEGER NOT NULL,
    completed_count INTEGER NOT NULL DEFAULT 0,
    censored_below_count INTEGER NOT NULL DEFAULT 0,
    censored_above_count INTEGER NOT NULL DEFAULT 0,
    duration_sum_ms REAL NOT NULL DEFAULT 0,
    duration_sum_squares_ms REAL NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (
        scope_kind,
        repository_id,
        fingerprint_id,
        operation,
        ecosystem,
        breadth,
        model_version,
        key_version
    )
);

CREATE INDEX validation_history_aggregates_updated_at_idx
    ON validation_history_aggregates(updated_at);
