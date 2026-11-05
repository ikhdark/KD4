-- Earlier backfills certified provisional rows despite missing session metadata.
-- Revisit their rollouts: filename-derived rows do not prove complete spawn edges.
-- Revoke any in-flight old claim so it cannot republish the invalid certificate.
UPDATE backfill_state
SET status = 'pending',
    last_watermark = NULL,
    last_success_at = NULL,
    claim_token = NULL;
