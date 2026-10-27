-- Reconciliation resolves physical rollout paths before falling back to thread IDs.
CREATE INDEX idx_threads_rollout_path ON threads(rollout_path);
