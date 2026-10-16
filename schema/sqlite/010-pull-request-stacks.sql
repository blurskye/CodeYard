ALTER TABLE pull_requests ADD COLUMN stack_id INTEGER CHECK (stack_id IS NULL OR typeof(stack_id)='integer');
CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_stack ON pull_requests(repo_id, stack_id);
