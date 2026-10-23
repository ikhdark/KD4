-- A mutable destination has one job owner. Revision acknowledgments alone are
-- not evidence that the file still contains that job's bytes.
CREATE TABLE agent_job_publications (
    destination TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL UNIQUE REFERENCES agent_jobs(id),
    revision INTEGER,
    sha256 TEXT
);
CREATE INDEX agent_job_publication_owner ON agent_job_publications(job_id);
