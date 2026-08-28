ALTER TABLE agent_jobs ADD COLUMN runner_instance_id TEXT;

CREATE INDEX idx_agent_jobs_runner_instance
    ON agent_jobs(status, runner_instance_id);

CREATE TABLE agent_job_runner_instances (
    runner_instance_id TEXT PRIMARY KEY,
    heartbeat_at INTEGER NOT NULL
);

CREATE INDEX idx_agent_job_runner_instances_heartbeat
    ON agent_job_runner_instances(heartbeat_at);
