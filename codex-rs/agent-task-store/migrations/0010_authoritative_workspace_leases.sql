ALTER TABLE workspace_mutation_leases
ADD COLUMN actor_kind TEXT NOT NULL DEFAULT '"legacy"';
