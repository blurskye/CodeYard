-- Pull requests take a milestone, as issues do. Existing pull requests have none.
ALTER TABLE pull_requests ADD COLUMN milestone_id BIGINT REFERENCES milestones(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_pull_requests_milestone_id ON pull_requests(milestone_id);
