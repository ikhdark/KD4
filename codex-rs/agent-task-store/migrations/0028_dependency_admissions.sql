-- Immutable admission-time prerequisite versions; this does not introduce a
-- continuous freshness/acceptance contract for already-admitted work.
CREATE TABLE dependency_admissions (
    assignment_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    dependency_id TEXT NOT NULL REFERENCES assignments(assignment_id),
    body_json TEXT NOT NULL,
    PRIMARY KEY (assignment_id, dependency_id)
);
CREATE TRIGGER dependency_admissions_immutable_update BEFORE UPDATE ON dependency_admissions
BEGIN SELECT RAISE(ABORT, 'dependency admissions are immutable'); END;
CREATE TRIGGER dependency_admissions_immutable_delete BEFORE DELETE ON dependency_admissions
BEGIN SELECT RAISE(ABORT, 'dependency admissions are immutable'); END;
