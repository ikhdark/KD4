-- archived_at may be reconstructed from the rollout mtime. Never use it alone
-- to authorize deletion. Legacy archives start with a full recovery window.
ALTER TABLE threads ADD COLUMN archive_observed_at INTEGER;
UPDATE threads SET archive_observed_at = unixepoch() WHERE archived = 1;
CREATE INDEX idx_threads_archive_clock ON threads(archive_observed_at) WHERE archived = 1;
CREATE TRIGGER threads_archive_clock_insert AFTER INSERT ON threads WHEN NEW.archived = 1
BEGIN
    UPDATE threads SET archive_observed_at = unixepoch() WHERE id = NEW.id;
END;
CREATE TRIGGER threads_archive_clock_change AFTER UPDATE OF archived ON threads
WHEN OLD.archived != NEW.archived
BEGIN
    UPDATE threads SET archive_observed_at = CASE WHEN NEW.archived = 1 THEN unixepoch() END WHERE id = NEW.id;
END;
