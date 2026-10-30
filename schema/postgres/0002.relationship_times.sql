ALTER TABLE issue_relations ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp();
CREATE INDEX issue_relations_added_source ON issue_relations(kind,source_id,created_at,target_id);
CREATE INDEX issue_relations_added_target ON issue_relations(kind,target_id,created_at,source_id);
