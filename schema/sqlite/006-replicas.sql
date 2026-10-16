-- Replicas: other storage nodes keep a copy of a repository, and the storage nodes
-- keep the copies up to date. write_version counts the ref changes made on the
-- repository's primary; a copy is current when its applied_version equals it.
ALTER TABLE repositories ADD COLUMN write_version INTEGER NOT NULL DEFAULT 0 CHECK (write_version >= 0);

-- '1' keeps one copy (the default), a count such as '2' keeps that many copies in
-- all, and 'all' keeps a copy on every storage node.
-- The table is rebuilt to add the setting and to drop cluster_count, which nothing read
-- (nodes join through invites). SQLite cannot drop a column that a table check names.
CREATE TABLE node_configuration_new (
    id INTEGER PRIMARY KEY CHECK (id IS NULL OR typeof(id)='integer') CHECK (id BETWEEN -32768 AND 32767),
    node_mode TEXT NOT NULL DEFAULT 'public' CHECK (length(node_mode)<=20),
    setup_completed INTEGER NOT NULL DEFAULT 0 CHECK (setup_completed IN (0,1)),
    replicas TEXT NOT NULL DEFAULT '1'
        CHECK (replicas = 'all' OR (replicas GLOB '[1-9]*' AND replicas NOT GLOB '*[^0-9]*' AND length(replicas) <= 3)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CONSTRAINT node_configuration_singleton CHECK (id = 1),
    CONSTRAINT node_configuration_mode_check CHECK (node_mode IN ('public', 'private'))
);
INSERT INTO node_configuration_new (id, node_mode, setup_completed, created_at, updated_at)
    SELECT id, node_mode, setup_completed, created_at, updated_at FROM node_configuration;
DROP TABLE node_configuration;
ALTER TABLE node_configuration_new RENAME TO node_configuration;

-- A copy held by another node. It goes with its repository. applied_version -1 means
-- nothing has been copied yet.
CREATE TABLE repository_replicas (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id),
    applied_version INTEGER NOT NULL DEFAULT -1 CHECK (applied_version >= -1),
    synced_at TEXT,
    PRIMARY KEY (repository_id, node_id)
);

CREATE INDEX idx_repository_replicas_node ON repository_replicas(node_id);
