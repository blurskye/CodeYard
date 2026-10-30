-- Match CI storage to public Git refs. The migration runner disables foreign keys
-- while rebuilding referenced tables, then checks the complete graph before commit.
CREATE TEMP TABLE ci_runs_sequence AS SELECT seq FROM sqlite_sequence WHERE name='ci_runs';
CREATE TABLE ci_runs_next (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    run_id BLOB NOT NULL UNIQUE CHECK (run_id IS NULL OR (typeof(run_id)='blob' AND length(run_id)=16)),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    triggered_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (triggered_by_id IS NULL OR typeof(triggered_by_id)='integer'),
    pull_request_id INTEGER REFERENCES pull_requests(id) ON DELETE SET NULL CHECK (pull_request_id IS NULL OR typeof(pull_request_id)='integer'),
    mode TEXT NOT NULL CHECK (length(mode)<=20),
    pipeline TEXT NOT NULL DEFAULT 'ci' CHECK (length(pipeline)<=20),
    event TEXT NOT NULL CHECK (length(event)<=50),
    is_fork INTEGER NOT NULL DEFAULT 0 CHECK (is_fork IN (0,1)),
    branch TEXT NOT NULL CHECK (length(branch)<=1024),
    head_sha TEXT CHECK (length(head_sha)<=40),
    workflow_path TEXT NOT NULL,
    environment TEXT CHECK (length(environment)<=255),
    status TEXT NOT NULL DEFAULT 'queued' CHECK (length(status)<=20),
    error_message TEXT,
    started_at TEXT,
    completed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    workflow_sha TEXT CHECK (length(workflow_sha)<=40),
    workflow_blobs TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(workflow_blobs)),
    worker TEXT,
    heartbeat_at TEXT,
    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0,1)),
    log_lines INTEGER NOT NULL DEFAULT 0 CHECK (log_lines >= 0),
    log_bytes INTEGER NOT NULL DEFAULT 0 CHECK (log_bytes >= 0),
    pull_request_event TEXT CHECK (pull_request_event IS NULL OR json_valid(pull_request_event)),
    workflow_inputs TEXT CHECK (workflow_inputs IS NULL OR json_valid(workflow_inputs)),
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    engine TEXT CHECK (engine IN ('codeyard-actions','act')),
    workflow_run_number INTEGER,
    workflow_run_id INTEGER REFERENCES ci_runs(id) ON DELETE CASCADE,
    workflow_run_attempt INTEGER,
    log_version INTEGER NOT NULL DEFAULT 0,
    runner_labels TEXT NOT NULL DEFAULT '[]',
    runner_group TEXT,
    approval_required BOOLEAN NOT NULL DEFAULT FALSE,
    event_payload TEXT
);
INSERT INTO ci_runs_next SELECT * FROM ci_runs;
DROP TRIGGER actions_run_delete;
DROP TRIGGER actions_github_tokens_delete;
DROP TABLE ci_runs;
ALTER TABLE ci_runs_next RENAME TO ci_runs;
UPDATE sqlite_sequence SET seq=max(seq,COALESCE((SELECT max(seq) FROM ci_runs_sequence),0)) WHERE name='ci_runs';
INSERT INTO sqlite_sequence(name,seq) SELECT 'ci_runs',seq FROM ci_runs_sequence WHERE NOT EXISTS(SELECT 1 FROM sqlite_sequence WHERE name='ci_runs');
DROP TABLE ci_runs_sequence;

CREATE UNIQUE INDEX idx_ci_runs_pull_revision ON ci_runs(pull_request_id,branch,head_sha,workflow_path,attempt) WHERE event='pull_request' AND pull_request_id IS NOT NULL;
CREATE INDEX idx_ci_runs_repo_created_at ON ci_runs(repo_id,created_at DESC);
CREATE INDEX idx_ci_runs_user_created_at ON ci_runs(triggered_by_id,created_at DESC);
CREATE INDEX idx_ci_runs_status ON ci_runs(status);
CREATE INDEX idx_ci_runs_push_user_created_at ON ci_runs(triggered_by_id,created_at DESC) WHERE event='push';
CREATE INDEX idx_ci_runs_queue ON ci_runs(status,id) WHERE status IN ('queued','running');
CREATE INDEX idx_ci_runs_repo_head ON ci_runs(repo_id,head_sha,id);
CREATE INDEX idx_ci_runs_pull_request ON ci_runs(pull_request_id) WHERE pull_request_id IS NOT NULL;
CREATE INDEX idx_ci_runs_push_created_at ON ci_runs(created_at) WHERE event='push' AND triggered_by_id IS NOT NULL;
CREATE INDEX ci_runs_workflow_run ON ci_runs(workflow_run_id,workflow_run_attempt);
CREATE TRIGGER actions_run_delete BEFORE DELETE ON ci_runs BEGIN
    DELETE FROM actions_objects WHERE run_id=OLD.id AND (kind='artifact' OR NOT finalized);
END;
CREATE TRIGGER actions_github_tokens_delete BEFORE DELETE ON ci_runs BEGIN
    DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM actions_github_tokens WHERE run_id=OLD.id);
END;
