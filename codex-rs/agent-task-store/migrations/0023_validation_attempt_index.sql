-- Task reconstruction and receipt sealing select one attempt from an append-only
-- history shared by every session. Keep those reads independent of other attempts.
-- The call-id suffix also supplies receipt sealing's deterministic ordering.
CREATE INDEX validation_calls_attempt_idx ON validation_calls(attempt_id, call_id);
