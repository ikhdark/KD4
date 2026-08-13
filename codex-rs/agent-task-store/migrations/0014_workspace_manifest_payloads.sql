CREATE TABLE workspace_manifest_payloads (
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    manifest_id TEXT NOT NULL,
    payload_format_version INTEGER NOT NULL,
    canonical_manifest_bytes BLOB NOT NULL,
    entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
    payload_byte_count INTEGER NOT NULL CHECK (payload_byte_count >= 0),
    created_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, manifest_id)
);

ALTER TABLE workspace_mutation_leases
ADD COLUMN expected_manifest_storage_kind TEXT NOT NULL DEFAULT 'inline_v1';

ALTER TABLE workspace_mutation_leases
ADD COLUMN expected_manifest_reference_hash TEXT;

ALTER TABLE workspace_repositories
ADD COLUMN head_manifest_json TEXT NOT NULL DEFAULT '[]';

ALTER TABLE workspace_repositories
ADD COLUMN head_manifest_storage_kind TEXT NOT NULL DEFAULT 'inline_v1';

ALTER TABLE workspace_repositories
ADD COLUMN head_manifest_reference_hash TEXT;
