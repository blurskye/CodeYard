-- Releases: one latest release per repository, and unique asset names within a release.
ALTER TABLE releases ADD COLUMN IF NOT EXISTS is_latest BOOLEAN NOT NULL DEFAULT FALSE;
CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_one_latest ON releases(repo_id) WHERE is_latest;
CREATE UNIQUE INDEX IF NOT EXISTS idx_release_assets_name ON release_assets(release_id, name);
