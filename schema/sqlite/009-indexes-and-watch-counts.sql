CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_file_id ON upload_rate_limit(file_id);
CREATE INDEX IF NOT EXISTS idx_download_rate_limit_file_id ON download_rate_limit(file_id);
CREATE INDEX IF NOT EXISTS idx_repositories_public_stars
    ON repositories(stars_count DESC, created_at DESC, id DESC) WHERE is_private = false;
CREATE INDEX IF NOT EXISTS idx_repositories_public_search
    ON repositories(stars_count DESC, updated_at DESC) WHERE NOT is_private;
CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at DESC);

DROP TRIGGER IF EXISTS watches_insert;
DROP TRIGGER IF EXISTS watches_delete;
DROP TRIGGER IF EXISTS watches_update;
CREATE TRIGGER watches_insert AFTER INSERT ON watches WHEN NEW.level != 'ignoring' BEGIN
    UPDATE repositories SET watchers_count=watchers_count+1 WHERE id=NEW.repo_id;
END;
CREATE TRIGGER watches_delete AFTER DELETE ON watches WHEN OLD.level != 'ignoring' BEGIN
    UPDATE repositories SET watchers_count=watchers_count-1 WHERE id=OLD.repo_id;
END;
CREATE TRIGGER watches_update AFTER UPDATE OF level,repo_id ON watches BEGIN
    UPDATE repositories SET watchers_count=watchers_count-1 WHERE id=OLD.repo_id AND OLD.level != 'ignoring';
    UPDATE repositories SET watchers_count=watchers_count+1 WHERE id=NEW.repo_id AND NEW.level != 'ignoring';
END;

UPDATE repositories SET watchers_count = (
    SELECT COUNT(*) FROM watches WHERE repo_id = repositories.id AND level != 'ignoring'
);
