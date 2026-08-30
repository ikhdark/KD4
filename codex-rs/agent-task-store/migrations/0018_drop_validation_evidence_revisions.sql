-- `validation_evidence_revisions` was introduced by 0015 for the completion
-- review subsystem, which has since been removed. No producer or consumer
-- reads the counter, but its triggers still fire an extra upsert on every
-- `validation_calls` insert and update. Drop the triggers first so the write
-- amplification stops, then drop the table.
DROP TRIGGER IF EXISTS validation_evidence_revision_insert;
DROP TRIGGER IF EXISTS validation_evidence_revision_update;

DROP TABLE IF EXISTS validation_evidence_revisions;
