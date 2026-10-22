-- Recovery is terminal: late workers cannot change the snapshot. Keep its export
-- obligation independently of job status, including across post-commit read errors.
CREATE TABLE agent_job_recovery_exports (
    job_id TEXT PRIMARY KEY NOT NULL REFERENCES agent_jobs(id),
    claim_token TEXT,
    retry_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX agent_job_recovery_exports_retry ON agent_job_recovery_exports(retry_at, job_id);
