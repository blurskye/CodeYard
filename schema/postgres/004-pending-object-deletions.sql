ALTER TABLE files ADD COLUMN put_expires_at TIMESTAMPTZ;
UPDATE files
SET put_expires_at = COALESCE(upload_expires_at, created_at + INTERVAL '900 seconds');
CREATE TABLE pending_object_deletions (
    object_key TEXT PRIMARY KEY,
    delete_after TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_pending_object_deletions_due
    ON pending_object_deletions(delete_after);
