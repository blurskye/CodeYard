-- Existing repositories keep every merge method, as they did before this setting existed.
ALTER TABLE repositories
    ADD COLUMN allow_merge_commit BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN allow_squash_merge BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN allow_rebase_merge BOOLEAN NOT NULL DEFAULT TRUE;
