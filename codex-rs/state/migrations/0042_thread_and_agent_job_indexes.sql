CREATE INDEX IF NOT EXISTS idx_agent_job_items_job_row
    ON agent_job_items(job_id, row_index ASC);

CREATE INDEX IF NOT EXISTS idx_threads_cwd_norm
    ON threads(lower(replace(cwd, '\', '/')));
