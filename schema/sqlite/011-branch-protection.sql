ALTER TABLE branch_protection_rules ADD COLUMN enforce_admins INTEGER NOT NULL DEFAULT 1
    CHECK (enforce_admins IN (0,1));
ALTER TABLE branch_protection_rules ADD COLUMN require_conversation_resolution INTEGER NOT NULL DEFAULT 0
    CHECK (require_conversation_resolution IN (0,1));
ALTER TABLE pr_comments ADD COLUMN resolved_at TEXT;
ALTER TABLE pr_comments ADD COLUMN resolved_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL
    CHECK (resolved_by_id IS NULL OR typeof(resolved_by_id)='integer');

-- Every existing rule already refuses all direct writes, owners included. Store that.
UPDATE branch_protection_rules SET require_pull_request = 1;
-- Approvals already count for the exact head only.
UPDATE branch_protection_rules SET dismiss_stale_reviews = 1;
-- A trailing `*` used to match any suffix, `/` included. That is `**` now; a single `*`
-- stops at `/`. Under the old matcher an `x**` rule only matched branch names that start
-- with a literal `x*`, so it protected nothing. Drop it when an `x*` rule is about to take
-- its name. Other `**` rows are left alone.
DELETE FROM branch_protection_rules WHERE pattern LIKE '%**' AND EXISTS (
    SELECT 1 FROM branch_protection_rules AS single
    WHERE single.repo_id = branch_protection_rules.repo_id
      AND single.pattern LIKE '%*' AND single.pattern NOT LIKE '%**'
      AND length(single.pattern) < 255
      AND single.pattern || '*' = branch_protection_rules.pattern);
UPDATE branch_protection_rules SET pattern = pattern || '*'
WHERE pattern LIKE '%*' AND pattern NOT LIKE '%**' AND length(pattern) < 255;
