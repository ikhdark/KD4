-- NULL remains fillable for legacy rows unless the corresponding explicit
-- clear bit is set: SHA = 1, branch = 2, origin URL = 4.
ALTER TABLE threads ADD COLUMN git_explicit_clear_mask INTEGER NOT NULL DEFAULT 0
    CHECK (git_explicit_clear_mask BETWEEN 0 AND 7);

-- Older writers leave the clear mask unchanged. Suppress their writes to
-- cleared fields, not the rest of their metadata statement. New explicit
-- replacements release authority in the same UPDATE as the new Git value.
CREATE TRIGGER thread_git_compatibility_keep_explicit_clears
AFTER UPDATE OF git_sha, git_branch, git_origin_url, git_explicit_clear_mask ON threads
WHEN ((NEW.git_explicit_clear_mask & 1) != 0 AND NEW.git_sha IS NOT NULL)
  OR ((NEW.git_explicit_clear_mask & 2) != 0 AND NEW.git_branch IS NOT NULL)
  OR ((NEW.git_explicit_clear_mask & 4) != 0 AND NEW.git_origin_url IS NOT NULL)
BEGIN
    UPDATE threads SET
        git_sha = CASE WHEN (NEW.git_explicit_clear_mask & 1) != 0 THEN NULL ELSE NEW.git_sha END,
        git_branch = CASE WHEN (NEW.git_explicit_clear_mask & 2) != 0 THEN NULL ELSE NEW.git_branch END,
        git_origin_url = CASE WHEN (NEW.git_explicit_clear_mask & 4) != 0 THEN NULL ELSE NEW.git_origin_url END
    WHERE id = NEW.id;
END;

CREATE TABLE thread_git_compatibility_outbox (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    rollout_path TEXT NOT NULL,
    session_id TEXT NOT NULL,
    record_version INTEGER NOT NULL,
    record BLOB NOT NULL,
    publication_checkpoint BLOB
);

CREATE INDEX idx_thread_git_compatibility_next
    ON thread_git_compatibility_outbox(thread_id, sequence);

-- Retire pending intents at the existing thread deletion boundary, including
-- deletes by older binaries (independent of connection PRAGMA foreign_keys).
CREATE TRIGGER thread_git_compatibility_delete_thread AFTER DELETE ON threads
BEGIN
    DELETE FROM thread_git_compatibility_outbox WHERE thread_id = OLD.id;
END;
