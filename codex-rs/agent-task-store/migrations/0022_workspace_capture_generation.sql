ALTER TABLE workspace_repositories
ADD COLUMN capture_generation INTEGER NOT NULL DEFAULT 0 CHECK (capture_generation >= 0);
