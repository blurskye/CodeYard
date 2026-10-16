-- Branch updates made by pushes, the web editor, the API and merges, for profile activity.
-- Each row is one branch update. Commit counts come from Git's own range count.
CREATE TABLE IF NOT EXISTS repository_pushes (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    actor_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (actor_id IS NULL OR typeof(actor_id)='integer'),
    branch TEXT NOT NULL CHECK (length(branch)<=255),
    commit_count INTEGER NOT NULL DEFAULT 0 CHECK (typeof(commit_count)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX IF NOT EXISTS idx_repository_pushes_actor ON repository_pushes(actor_id, created_at);

-- The user who created each repository, so an organization repository is listed under its
-- creator. Repositories created before this migration have none and fall back to their owner.
-- No foreign key: SQLite cannot drop a column that has one. A deleted user's id matches no one.
ALTER TABLE repositories ADD COLUMN created_by_id INTEGER CHECK (created_by_id IS NULL OR typeof(created_by_id)='integer');
CREATE INDEX IF NOT EXISTS idx_repositories_created_by ON repositories(created_by_id);
