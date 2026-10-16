-- SQLite needs a table rebuild to remove the released context-length check.
-- No tables reference commit_statuses; foreign keys remain enabled throughout.
CREATE TABLE commit_statuses_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    sha TEXT NOT NULL CHECK (length(sha)<=40),
    context TEXT NOT NULL,
    state TEXT NOT NULL CHECK (length(state)<=20),
    description TEXT,
    target_url TEXT,
    creator_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (creator_id IS NULL OR typeof(creator_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    ci_run_id INTEGER REFERENCES ci_runs(id) ON DELETE SET NULL CHECK (ci_run_id IS NULL OR typeof(ci_run_id)='integer'),
    UNIQUE(repo_id, sha, context)
);

INSERT INTO commit_statuses_new(id, repo_id, sha, context, state, description, target_url, creator_id, created_at, updated_at, ci_run_id)
SELECT id, repo_id, sha, context, state, description, target_url, creator_id, created_at, updated_at, ci_run_id FROM commit_statuses;

-- Keep AUTOINCREMENT's last id even when the last status was deleted.
DELETE FROM sqlite_sequence WHERE name = 'commit_statuses_new';
INSERT INTO sqlite_sequence(name, seq)
SELECT 'commit_statuses_new', seq FROM sqlite_sequence WHERE name = 'commit_statuses';

DROP TABLE commit_statuses;
ALTER TABLE commit_statuses_new RENAME TO commit_statuses;
CREATE INDEX idx_commit_statuses_repo_sha ON commit_statuses(repo_id, sha);
