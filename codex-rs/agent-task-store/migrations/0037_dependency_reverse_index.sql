-- Root freshness triggers traverse admitted dependents by dependency identity.
-- The immutable table's primary key starts with assignment_id, not dependency_id.
CREATE INDEX dependency_admissions_dependency_idx
ON dependency_admissions(dependency_id, assignment_id);
