-- Immutable acknowledgments survive response loss without replaying effects.
CREATE TABLE agent_job_report_receipts (
    job_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    attempt_count INTEGER NOT NULL,
    thread_id TEXT NOT NULL,
    result_json TEXT NOT NULL,
    cancel_job INTEGER NOT NULL,
    cancellation_reason TEXT NOT NULL,
    reported_at INTEGER NOT NULL,
    PRIMARY KEY (job_id, item_id),
    FOREIGN KEY (job_id, item_id) REFERENCES agent_job_items(job_id, item_id) ON DELETE CASCADE
);
