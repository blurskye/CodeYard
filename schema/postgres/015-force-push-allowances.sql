-- Users who may force push to a branch whose rule blocks force pushes.
ALTER TABLE branch_protection_rules ADD COLUMN force_push_allowances BIGINT[] DEFAULT '{}';
