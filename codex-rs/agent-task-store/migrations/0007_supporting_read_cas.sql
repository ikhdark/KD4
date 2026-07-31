CREATE TABLE actor_supporting_reads (
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    actor_id TEXT NOT NULL,
    path TEXT NOT NULL,
    manifest_entry_json TEXT NOT NULL,
    read_epoch INTEGER NOT NULL CHECK (read_epoch >= 0),
    read_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, actor_id, path)
);

CREATE INDEX actor_supporting_reads_actor_idx
ON actor_supporting_reads(workspace_id, actor_id, read_at);
