CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_file_id ON upload_rate_limit(file_id);
CREATE INDEX IF NOT EXISTS idx_download_rate_limit_file_id ON download_rate_limit(file_id);
CREATE INDEX IF NOT EXISTS idx_repositories_public_stars
    ON repositories(stars_count DESC, created_at DESC, id DESC) WHERE is_private = false;

CREATE OR REPLACE FUNCTION update_repo_watchers_count() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP IN ('DELETE', 'UPDATE') AND OLD.level != 'ignoring' THEN
        UPDATE repositories SET watchers_count = watchers_count - 1 WHERE id = OLD.repo_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') AND NEW.level != 'ignoring' THEN
        UPDATE repositories SET watchers_count = watchers_count + 1 WHERE id = NEW.repo_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trigger_update_watchers_count ON watches;
CREATE TRIGGER trigger_update_watchers_count
    AFTER INSERT OR DELETE OR UPDATE OF level, repo_id ON watches
    FOR EACH ROW EXECUTE FUNCTION update_repo_watchers_count();

UPDATE repositories SET watchers_count = (
    SELECT COUNT(*) FROM watches WHERE repo_id = repositories.id AND level != 'ignoring'
);
