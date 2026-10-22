-- Retain publication revisions separately from execution outcome. Do not replay
-- historical terminal jobs: only existing obligations and admitted live work.
ALTER TABLE agent_job_recovery_exports ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE agent_job_recovery_exports ADD COLUMN published_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agent_job_recovery_exports ADD COLUMN claim_revision INTEGER;
CREATE INDEX agent_job_unpublished_exports ON agent_job_recovery_exports(retry_at, job_id)
WHERE revision > published_revision;
INSERT OR IGNORE INTO agent_job_recovery_exports (job_id)
SELECT id FROM agent_jobs WHERE auto_export = 1 AND status IN ('pending', 'running');

CREATE TRIGGER agent_job_export_created AFTER INSERT ON agent_jobs
WHEN NEW.auto_export = 1 BEGIN
    INSERT INTO agent_job_recovery_exports (job_id) VALUES (NEW.id);
END;
CREATE TRIGGER agent_job_export_terminal AFTER UPDATE OF status ON agent_jobs
WHEN NEW.auto_export = 1 AND NEW.status IN ('completed', 'failed', 'cancelled') BEGIN
    INSERT INTO agent_job_recovery_exports (job_id) VALUES (NEW.id)
    ON CONFLICT(job_id) DO UPDATE SET revision = revision + 1,
        retry_at = CASE WHEN claim_token IS NULL THEN 0 ELSE retry_at END;
END;
-- Initial rows and revision 1 commit together in create_agent_job. There is no
-- incremental row-admission API, so do not update the queue once per input row.
CREATE TRIGGER agent_job_export_item_updated AFTER UPDATE ON agent_job_items BEGIN
    UPDATE agent_job_recovery_exports SET revision = revision + 1,
        retry_at = CASE WHEN claim_token IS NULL THEN 0 ELSE retry_at END WHERE job_id = NEW.job_id;
END;
CREATE TRIGGER agent_job_export_item_deleted AFTER DELETE ON agent_job_items BEGIN
    UPDATE agent_job_recovery_exports SET revision = revision + 1,
        retry_at = CASE WHEN claim_token IS NULL THEN 0 ELSE retry_at END WHERE job_id = OLD.job_id;
END;

-- This is evidence, not a live assignment or a foreign key to a deletable rollout.
CREATE TABLE agent_job_attempts (
    job_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    attempt_count INTEGER NOT NULL,
    thread_id TEXT NOT NULL,
    bound_at INTEGER NOT NULL,
    PRIMARY KEY (job_id, item_id, attempt_count),
    FOREIGN KEY (job_id, item_id) REFERENCES agent_job_items(job_id, item_id) ON DELETE CASCADE
);
CREATE TRIGGER agent_job_attempt_bound AFTER UPDATE OF assigned_thread_id ON agent_job_items
WHEN NEW.assigned_thread_id IS NOT NULL AND NEW.assigned_thread_id IS NOT OLD.assigned_thread_id BEGIN
    INSERT INTO agent_job_attempts (job_id, item_id, attempt_count, thread_id, bound_at)
    VALUES (NEW.job_id, NEW.item_id, NEW.attempt_count, NEW.assigned_thread_id, NEW.updated_at)
    ON CONFLICT(job_id, item_id, attempt_count) DO UPDATE SET thread_id = excluded.thread_id, bound_at = excluded.bound_at;
END;
