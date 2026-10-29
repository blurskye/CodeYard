ALTER TABLE code_scanning_uploads ADD COLUMN claim_id TEXT;
ALTER TABLE code_scanning_uploads ADD COLUMN claim_until TEXT;

CREATE INDEX idx_repository_replicas_pending ON repository_replicas(node_id, repository_id)
WHERE applied_version < 0 AND synced_at IS NULL;

INSERT INTO file_reason_limits (reason, max_file_size_bytes, max_uploads_per_hour, max_bytes_per_hour, max_uploads_per_day, max_bytes_per_day, allowed_content_types, description)
VALUES ('package', 104857600, 100, 1073741824, 500, 5368709120, NULL, 'Package blob uploads')
ON CONFLICT (reason) DO NOTHING;
