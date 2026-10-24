-- observations already owns every wake identity and payload. Retain the legacy
-- table/index schema for compatibility, but retire its redundant cached bodies.
-- Bounded retention and stream freshness remain in wake_streams.
DELETE FROM wake_events;
