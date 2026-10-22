-- Bounded status projection independent of the general observation tail.
CREATE INDEX observations_attempt_kind_sequence_idx
    ON observations(attempt_id, kind, sequence DESC);
