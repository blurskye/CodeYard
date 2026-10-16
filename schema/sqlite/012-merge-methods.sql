-- Existing repositories keep every merge method, as they did before this setting existed.
ALTER TABLE repositories ADD COLUMN allow_merge_commit INTEGER NOT NULL DEFAULT 1
    CHECK (allow_merge_commit IN (0,1));
ALTER TABLE repositories ADD COLUMN allow_squash_merge INTEGER NOT NULL DEFAULT 1
    CHECK (allow_squash_merge IN (0,1));
ALTER TABLE repositories ADD COLUMN allow_rebase_merge INTEGER NOT NULL DEFAULT 1
    CHECK (allow_rebase_merge IN (0,1));
