-- no-transaction
-- These indexes duplicate the UNIQUE/PRIMARY KEY indexes SQLite already keeps on
-- (root_session_id, wake_sequence). Every observation append and wake retention
-- delete maintained a second identical B-tree without changing any query plan.
DROP INDEX IF EXISTS observations_wake_idx;
DROP INDEX IF EXISTS wake_events_root_sequence_idx;

-- 0016 dropped the retired workspace tables, but auto_vacuum is off, so their
-- pages stayed on the freelist and upgraded stores kept hundreds of MB of dead
-- space. VACUUM cannot run inside a transaction and is safe to repeat if the
-- process exits before sqlx records this version.
VACUUM;
