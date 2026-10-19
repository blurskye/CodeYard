-- Releases: one latest release per repository, and unique asset names within a release.
ALTER TABLE releases ADD COLUMN is_latest INTEGER NOT NULL DEFAULT 0 CHECK (is_latest IN (0,1));
CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_one_latest ON releases(repo_id) WHERE is_latest = 1;
CREATE UNIQUE INDEX IF NOT EXISTS idx_release_assets_name ON release_assets(release_id, name);
