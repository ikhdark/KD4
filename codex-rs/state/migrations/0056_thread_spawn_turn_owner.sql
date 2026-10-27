ALTER TABLE thread_spawn_edges ADD COLUMN parent_turn_id TEXT;
ALTER TABLE thread_spawn_edges ADD COLUMN inherited_user_instructions TEXT;
CREATE INDEX idx_thread_spawn_edges_turn_owner
    ON thread_spawn_edges(parent_thread_id, parent_turn_id);
