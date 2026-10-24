-- Suite launch ownership lives in the root turn, not the coordination database.
-- Keep migration 29 intact so existing databases retain valid SQLx checksums.
DROP TABLE IF EXISTS validation_task_threads;
DROP TABLE IF EXISTS validation_suite_grants;
