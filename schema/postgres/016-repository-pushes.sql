-- Branch updates made by pushes, the web editor, the API and merges, for profile activity.
-- Each row is one branch update. Commit counts come from Git's own range count.
CREATE TABLE IF NOT EXISTS repository_pushes (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    branch VARCHAR(255) NOT NULL,
    commit_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_repository_pushes_actor ON repository_pushes(actor_id, created_at);

-- The user who created each repository, so an organization repository is listed under its
-- creator. Repositories created before this migration have none and fall back to their owner.
ALTER TABLE repositories ADD COLUMN created_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_repositories_created_by ON repositories(created_by_id);
