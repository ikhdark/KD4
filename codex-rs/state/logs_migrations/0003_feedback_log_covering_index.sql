CREATE INDEX IF NOT EXISTS idx_logs_feedback_thread_ts
    ON logs(thread_id, ts DESC, ts_nanos DESC, id DESC)
    WHERE feedback_log_body IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_logs_feedback_process_threadless_ts
    ON logs(process_uuid, ts DESC, ts_nanos DESC, id DESC)
    WHERE thread_id IS NULL AND feedback_log_body IS NOT NULL;
