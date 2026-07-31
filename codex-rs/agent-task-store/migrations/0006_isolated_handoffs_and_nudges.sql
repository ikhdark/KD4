ALTER TABLE workspace_actors
ADD COLUMN nudge_sent_at TEXT;

CREATE TABLE isolated_handoffs (
    assignment_id TEXT PRIMARY KEY NOT NULL REFERENCES assignments(assignment_id),
    source_workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    source_epoch INTEGER NOT NULL CHECK (source_epoch >= 0),
    source_manifest_hash TEXT NOT NULL,
    covered_manifest_json TEXT NOT NULL,
    state TEXT NOT NULL,
    integrator_assignment_id TEXT REFERENCES assignments(assignment_id),
    created_at TEXT NOT NULL,
    integrated_at TEXT
);

CREATE INDEX isolated_handoffs_integrator_idx
ON isolated_handoffs(integrator_assignment_id, state);
