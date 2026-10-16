-- Canonical Git refs can contain 1024 bytes. Replace only the column so repository
-- rows, their dependents and the baseline migration checksum stay intact.
ALTER TABLE repositories ADD COLUMN default_branch_new TEXT NOT NULL DEFAULT 'main'
    CHECK (length(default_branch_new) <= 1024);
UPDATE repositories SET default_branch_new = default_branch;
ALTER TABLE repositories DROP COLUMN default_branch;
ALTER TABLE repositories RENAME COLUMN default_branch_new TO default_branch;
