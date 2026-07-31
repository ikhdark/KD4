ALTER TABLE stale_recovery
ADD COLUMN last_stale_epoch INTEGER CHECK (last_stale_epoch >= 0);
