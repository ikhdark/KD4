CREATE TABLE bug_git_enrichment (
    bug_id INTEGER PRIMARY KEY REFERENCES bugs(id),
    repository_root TEXT,
    git_commit TEXT,
    captured_at INTEGER NOT NULL
);

CREATE TRIGGER bug_git_enrichment_immutable
BEFORE UPDATE ON bug_git_enrichment
BEGIN
    SELECT RAISE(ABORT, 'immutable bug enrichment evidence');
END;
