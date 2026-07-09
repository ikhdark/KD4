CREATE INDEX IF NOT EXISTS idx_jobs_kind_status_lease
    ON jobs(kind, status, lease_until);

CREATE INDEX IF NOT EXISTS idx_stage1_outputs_phase2_selection
    ON stage1_outputs(
        usage_count DESC,
        last_usage DESC,
        source_updated_at DESC,
        thread_id DESC
    );
