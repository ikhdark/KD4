-- Lease expiry transfers authority, not just the ability to start a worker.
ALTER TABLE backfill_state ADD COLUMN claim_token TEXT;
