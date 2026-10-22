-- A compact cross-connection fence for reusing already hydrated wait owners.
CREATE TABLE task_state_revision (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL);
INSERT INTO task_state_revision VALUES (1, 0);

CREATE TRIGGER task_revision_assignments_insert AFTER INSERT ON assignments
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_assignments_update AFTER UPDATE ON assignments
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_assignments_delete AFTER DELETE ON assignments
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_attempts_insert AFTER INSERT ON attempts
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_attempts_update AFTER UPDATE ON attempts
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_attempts_delete AFTER DELETE ON attempts
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_receipts_insert AFTER INSERT ON receipts
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_receipts_update AFTER UPDATE ON receipts
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_receipts_delete AFTER DELETE ON receipts
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_validation_calls_insert AFTER INSERT ON validation_calls
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_validation_calls_update AFTER UPDATE ON validation_calls
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_validation_calls_delete AFTER DELETE ON validation_calls
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_gates_insert AFTER INSERT ON gates
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_gates_update AFTER UPDATE ON gates
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_gates_delete AFTER DELETE ON gates
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_workspace_actors_insert AFTER INSERT ON workspace_actors
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_workspace_actors_update AFTER UPDATE ON workspace_actors
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_workspace_actors_delete AFTER DELETE ON workspace_actors
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_agent_task_bindings_insert AFTER INSERT ON agent_task_bindings
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_agent_task_bindings_update AFTER UPDATE ON agent_task_bindings
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_agent_task_bindings_delete AFTER DELETE ON agent_task_bindings
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_assignment_repositories_insert AFTER INSERT ON assignment_repositories
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_assignment_repositories_update AFTER UPDATE ON assignment_repositories
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER task_revision_assignment_repositories_delete AFTER DELETE ON assignment_repositories
BEGIN
    UPDATE task_state_revision SET revision = revision + 1 WHERE id = 1;
END;
