CREATE TABLE issue_relations_new (
    kind TEXT NOT NULL CHECK (kind IN ('parent','blocks','related')),
    source_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE CHECK (typeof(source_id)='integer'),
    target_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE CHECK (typeof(target_id)='integer'),
    position INTEGER NOT NULL DEFAULT 0 CHECK (typeof(position)='integer' AND position>=0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(kind,source_id,target_id),
    CHECK (source_id<>target_id),
    CHECK (kind<>'related' OR source_id<target_id)
);
INSERT INTO issue_relations_new(kind,source_id,target_id,position)
    SELECT kind,source_id,target_id,position FROM issue_relations;
DROP TABLE issue_relations;
ALTER TABLE issue_relations_new RENAME TO issue_relations;
CREATE UNIQUE INDEX issue_one_parent ON issue_relations(target_id) WHERE kind='parent';
CREATE INDEX issue_relations_target ON issue_relations(kind,target_id);
CREATE INDEX issue_relations_added_source ON issue_relations(kind,source_id,created_at,target_id);
CREATE INDEX issue_relations_added_target ON issue_relations(kind,target_id,created_at,source_id);
