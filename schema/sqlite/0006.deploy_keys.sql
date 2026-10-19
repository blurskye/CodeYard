-- Deploy keys become usable: the baseline table gains the account that added each key, which its
-- pushes are attributed to. A key stays unique across account keys and deploy keys; the insert
-- checks that, and the baseline UNIQUE(repo_id, fingerprint) only guards the same repository.
ALTER TABLE deploy_keys ADD COLUMN created_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (created_by_id IS NULL OR typeof(created_by_id)='integer');
