-- Canonical Git refs can contain 1024 bytes.
ALTER TABLE repositories ALTER COLUMN default_branch TYPE VARCHAR(1024);
