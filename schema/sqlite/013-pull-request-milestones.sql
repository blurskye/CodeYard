-- Pull requests take a milestone, as issues do. Existing pull requests have none.
ALTER TABLE pull_requests ADD COLUMN milestone_id INTEGER REFERENCES milestones(id) ON DELETE SET NULL CHECK (milestone_id IS NULL OR typeof(milestone_id)='integer');
CREATE INDEX IF NOT EXISTS idx_pull_requests_milestone_id ON pull_requests(milestone_id);
