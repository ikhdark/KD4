-- Inert compatibility storage for old general thread metadata writers.
-- This does not restore the retired memory feature or its lost eligibility.
ALTER TABLE threads ADD COLUMN memory_mode TEXT NOT NULL DEFAULT 'enabled';
