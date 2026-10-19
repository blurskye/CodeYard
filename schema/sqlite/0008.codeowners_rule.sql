-- Branch protection can require a review from each changed file's code owners. Existing rules keep the
-- old behaviour.
ALTER TABLE branch_protection_rules ADD COLUMN require_code_owner_review INTEGER NOT NULL DEFAULT 0 CHECK (require_code_owner_review IN (0,1));
