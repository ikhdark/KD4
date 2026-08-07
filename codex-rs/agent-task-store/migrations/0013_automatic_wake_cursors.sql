CREATE TABLE automatic_wake_cursors (
    root_session_id TEXT NOT NULL,
    consuming_agent_path TEXT NOT NULL,
    event_id TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (root_session_id, consuming_agent_path)
);
