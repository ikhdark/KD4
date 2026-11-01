-- NULL/empty derived titles cannot distinguish a legacy fallback from an explicit clear.
-- Existing rows keep their legacy compatibility behavior until explicitly renamed/cleared.
ALTER TABLE threads ADD COLUMN title_is_explicit INTEGER NOT NULL DEFAULT 0;
