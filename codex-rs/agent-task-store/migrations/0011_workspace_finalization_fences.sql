CREATE TABLE workspace_finalization_fences (
    fence_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspace_repositories(workspace_id),
    root_session_id TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT
);

CREATE UNIQUE INDEX workspace_finalization_fences_active_workspace_idx
ON workspace_finalization_fences(workspace_id)
WHERE state = 'active';

CREATE INDEX workspace_finalization_fences_expiry_idx
ON workspace_finalization_fences(workspace_id, state, expires_at);

CREATE TRIGGER finalization_blocks_assignment_repository_insert
BEFORE INSERT ON assignment_repositories
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = NEW.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_binding_insert
BEFORE INSERT ON agent_task_bindings
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = NEW.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_binding_update
BEFORE UPDATE ON agent_task_bindings
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = NEW.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_binding_delete
BEFORE DELETE ON agent_task_bindings
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = OLD.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_attempt_insert
BEFORE INSERT ON attempts
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = NEW.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_attempt_update
BEFORE UPDATE ON attempts
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = OLD.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_attempt_delete
BEFORE DELETE ON attempts
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = OLD.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_receipt_insert
BEFORE INSERT ON receipts
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = NEW.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_receipt_update
BEFORE UPDATE ON receipts
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = OLD.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_receipt_delete
BEFORE DELETE ON receipts
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = OLD.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_gate_insert
BEFORE INSERT ON gates
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = NEW.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_gate_update
BEFORE UPDATE ON gates
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = NEW.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_gate_delete
BEFORE DELETE ON gates
WHEN EXISTS (
    SELECT 1
    FROM assignment_repositories AS repository
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE repository.assignment_id = OLD.assignment_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_validation_insert
BEFORE INSERT ON validation_calls
WHEN EXISTS (
    SELECT 1
    FROM attempts AS attempt
    JOIN assignment_repositories AS repository
      ON repository.assignment_id = attempt.assignment_id
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE attempt.attempt_id = NEW.attempt_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_validation_update
BEFORE UPDATE ON validation_calls
WHEN EXISTS (
    SELECT 1
    FROM attempts AS attempt
    JOIN assignment_repositories AS repository
      ON repository.assignment_id = attempt.assignment_id
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE attempt.attempt_id = NEW.attempt_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;


CREATE TRIGGER finalization_blocks_validation_delete
BEFORE DELETE ON validation_calls
WHEN EXISTS (
    SELECT 1
    FROM attempts AS attempt
    JOIN assignment_repositories AS repository
      ON repository.assignment_id = attempt.assignment_id
    JOIN workspace_finalization_fences AS fence
      ON fence.workspace_id = repository.workspace_id
    WHERE attempt.attempt_id = OLD.attempt_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_assignment_repository_delete
BEFORE DELETE ON assignment_repositories
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_actor_insert
BEFORE INSERT ON workspace_actors
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = NEW.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_actor_update
BEFORE UPDATE ON workspace_actors
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_actor_delete
BEFORE DELETE ON workspace_actors
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_repository_update
BEFORE UPDATE ON workspace_repositories
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_repository_delete
BEFORE DELETE ON workspace_repositories
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_path_insert
BEFORE INSERT ON workspace_paths
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = NEW.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_path_update
BEFORE UPDATE ON workspace_paths
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_path_delete
BEFORE DELETE ON workspace_paths
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_event_insert
BEFORE INSERT ON workspace_events
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = NEW.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_event_update
BEFORE UPDATE ON workspace_events
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_workspace_event_delete
BEFORE DELETE ON workspace_events
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_supporting_read_insert
BEFORE INSERT ON actor_supporting_reads
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = NEW.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_supporting_read_update
BEFORE UPDATE ON actor_supporting_reads
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;

CREATE TRIGGER finalization_blocks_supporting_read_delete
BEFORE DELETE ON actor_supporting_reads
WHEN EXISTS (
    SELECT 1 FROM workspace_finalization_fences AS fence
    WHERE fence.workspace_id = OLD.workspace_id AND fence.state = 'active'
)
BEGIN
    SELECT RAISE(ABORT, 'workspace finalization active');
END;
