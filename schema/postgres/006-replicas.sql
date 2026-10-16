-- Replicas: other storage nodes keep a copy of a repository, and the storage nodes
-- keep the copies up to date. write_version counts the ref changes made on the
-- repository's primary; a copy is current when its applied_version equals it.
ALTER TABLE repositories ADD COLUMN write_version BIGINT NOT NULL DEFAULT 0 CHECK (write_version >= 0);

-- '1' keeps one copy (the default), a count such as '2' keeps that many copies in
-- all, and 'all' keeps a copy on every storage node.
ALTER TABLE node_configuration ADD COLUMN replicas TEXT NOT NULL DEFAULT '1'
    CHECK (replicas = 'all' OR replicas ~ '^[1-9][0-9]{0,2}$');

-- Nothing read the cluster size; nodes join through invites.
ALTER TABLE node_configuration DROP COLUMN IF EXISTS cluster_count;

-- A copy held by another node. It goes with its repository. applied_version -1 means
-- nothing has been copied yet.
CREATE TABLE repository_replicas (
    repository_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    node_id BIGINT NOT NULL REFERENCES nodes(id),
    applied_version BIGINT NOT NULL DEFAULT -1 CHECK (applied_version >= -1),
    synced_at TIMESTAMPTZ,
    PRIMARY KEY (repository_id, node_id)
);

CREATE INDEX idx_repository_replicas_node ON repository_replicas(node_id);
