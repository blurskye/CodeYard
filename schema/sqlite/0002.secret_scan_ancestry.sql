-- Earlier markers described only a commit's tree, not completion of its ancestry.
ALTER TABLE secret_scanning_commits ADD COLUMN ancestry_complete BOOLEAN NOT NULL DEFAULT FALSE;
