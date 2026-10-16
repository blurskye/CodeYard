-- Users who may force push to a branch whose rule blocks force pushes. Like push_allowances,
-- it is a JSON array of user ids.
ALTER TABLE branch_protection_rules ADD COLUMN force_push_allowances TEXT DEFAULT '[]'
    CHECK (force_push_allowances IS NULL OR (json_valid(force_push_allowances) AND json_type(force_push_allowances)='array'));
