ALTER TABLE files ADD COLUMN put_expires_at TEXT;
UPDATE files
SET put_expires_at = COALESCE(
    upload_expires_at,
    strftime('%Y-%m-%dT%H:%M:%f000Z', created_at, '+900 seconds')
);

CREATE TABLE pending_object_deletions (
    object_key TEXT PRIMARY KEY,
    delete_after TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE INDEX idx_pending_object_deletions_due
    ON pending_object_deletions(delete_after);
