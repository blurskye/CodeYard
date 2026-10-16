ALTER TABLE ci_runs
    ADD COLUMN pull_request_id BIGINT REFERENCES pull_requests(id) ON DELETE SET NULL;

DROP INDEX idx_ci_runs_pull_revision;
CREATE UNIQUE INDEX idx_ci_runs_pull_revision
    ON ci_runs(pull_request_id, branch, head_sha, workflow_path)
    WHERE event = 'pull_request' AND pull_request_id IS NOT NULL;
