-- Keep historical migration checksums intact while retiring memory eligibility.
ALTER TABLE threads DROP COLUMN memory_mode;
