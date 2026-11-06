-- The directory is a separate PostgreSQL database, with its own unreleased baseline.
CREATE TABLE directory_cells (
    id INTEGER PRIMARY KEY CHECK(id BETWEEN 0 AND 65535),
    name TEXT NOT NULL UNIQUE CHECK(octet_length(name) BETWEEN 1 AND 100),
    region TEXT NOT NULL CHECK(octet_length(region) BETWEEN 1 AND 63),
    state TEXT NOT NULL CHECK(state IN ('open','closed','disabled')),
    web_urls JSONB NOT NULL CHECK(jsonb_typeof(web_urls)='array' AND jsonb_array_length(web_urls) BETWEEN 1 AND 1000 AND octet_length(web_urls::text)<=4194304),
    ssh_addresses JSONB NOT NULL CHECK(jsonb_typeof(ssh_addresses)='array' AND jsonb_array_length(ssh_addresses)<=1000 AND octet_length(ssh_addresses::text)<=4194304),
    rpc_addresses JSONB NOT NULL CHECK(jsonb_typeof(rpc_addresses)='array' AND jsonb_array_length(rpc_addresses)<=1000 AND octet_length(rpc_addresses::text)<=4194304),
    database_bytes BIGINT NOT NULL CHECK(database_bytes>=0),
    write_rate BIGINT NOT NULL CHECK(write_rate>=0),
    maximum_database_bytes BIGINT NOT NULL CHECK(maximum_database_bytes BETWEEN 1 AND 4000000000000),
    maximum_write_rate BIGINT NOT NULL CHECK(maximum_write_rate BETWEEN 1 AND 10000),
    observed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX directory_cells_placement ON directory_cells(region,state,database_bytes,id);
CREATE TABLE directory_cell_prefixes (
    prefix INTEGER PRIMARY KEY CHECK(prefix BETWEEN 0 AND 65535),
    cell_id INTEGER NOT NULL REFERENCES directory_cells(id),
    previous_prefix INTEGER UNIQUE REFERENCES directory_cell_prefixes(prefix),
    CHECK(previous_prefix IS NULL OR previous_prefix<>prefix)
);
CREATE TABLE directory_node_keys (
    public_key TEXT PRIMARY KEY CHECK(length(public_key)=64),
    cell_id INTEGER NOT NULL REFERENCES directory_cells(id)
);
CREATE INDEX directory_node_keys_cell ON directory_node_keys(cell_id,public_key);
-- Routers can read routes but cannot claim names or report a cell's load.
CREATE TABLE directory_router_keys (
    public_key TEXT PRIMARY KEY CHECK(length(public_key)=64)
);
-- One home mapping stays stable when a name changes and switches atomically during a move.
CREATE TABLE directory_objects (
    kind TEXT NOT NULL CHECK(kind IN ('user','organization','repository')),
    id BIGINT NOT NULL CHECK(id BETWEEN 1 AND 9007199254740991),
    home_cell INTEGER NOT NULL REFERENCES directory_cells(id),
    active BOOLEAN NOT NULL DEFAULT FALSE,
    claim_count INTEGER NOT NULL DEFAULT 0 CHECK(claim_count BETWEEN 0 AND 1000),
    nonce_hash TEXT NOT NULL CHECK(length(nonce_hash)=64),
    expires_at TIMESTAMPTZ,
    PRIMARY KEY(kind,id),
    CHECK((active AND expires_at IS NULL) OR (NOT active AND expires_at IS NOT NULL))
);
CREATE INDEX directory_objects_expiry ON directory_objects(expires_at) WHERE NOT active;
CREATE TABLE directory_claims (
    shard SMALLINT NOT NULL CHECK(shard BETWEEN 0 AND 255),
    kind TEXT NOT NULL CHECK(kind IN ('name','email','ssh')),
    key_hash BYTEA NOT NULL CHECK(octet_length(key_hash)=32),
    value TEXT NOT NULL CHECK(octet_length(value) BETWEEN 1 AND 1024),
    object_kind TEXT NOT NULL,
    object_id BIGINT NOT NULL,
    nonce_hash TEXT NOT NULL CHECK(length(nonce_hash)=64),
    confirmed BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at TIMESTAMPTZ,
    PRIMARY KEY(shard,kind,key_hash),
    FOREIGN KEY(object_kind,object_id) REFERENCES directory_objects(kind,id) ON DELETE CASCADE,
    CHECK((confirmed AND expires_at IS NULL) OR (NOT confirmed AND expires_at IS NOT NULL))
);
CREATE INDEX directory_claims_object ON directory_claims(object_kind,object_id,nonce_hash);
CREATE INDEX directory_claims_expiry ON directory_claims(expires_at) WHERE NOT confirmed;

-- The parent count bounds every per-object operation, including hostile repeated batches.
CREATE FUNCTION directory_claim_count() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP IN ('DELETE','UPDATE') THEN
        UPDATE directory_objects SET claim_count=claim_count-1 WHERE kind=OLD.object_kind AND id=OLD.object_id;
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        UPDATE directory_objects SET claim_count=claim_count+1 WHERE kind=NEW.object_kind AND id=NEW.object_id;
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER directory_claim_count AFTER INSERT OR DELETE OR UPDATE OF object_kind,object_id ON directory_claims FOR EACH ROW EXECUTE FUNCTION directory_claim_count();

-- The global search index. Each cell writes its own repositories' documents through its
-- outbox; a later (event, attempt) replaces an earlier one, and a removed repository keeps
-- an empty row so a late write cannot bring it back. Private documents are returned only to
-- the readers their cell names.
CREATE TABLE search_repositories (
    id BIGINT PRIMARY KEY CHECK(id BETWEEN 1 AND 9007199254740991),
    cell_id INTEGER NOT NULL REFERENCES directory_cells(id),
    event_id BIGINT NOT NULL,
    attempt BIGINT NOT NULL,
    document JSONB CHECK(document IS NULL OR (jsonb_typeof(document)='object' AND octet_length(document::text)<=524288)),
    public BOOLEAN NOT NULL DEFAULT FALSE,
    classic_allowed BOOLEAN NOT NULL DEFAULT TRUE,
    code_head TEXT CHECK(code_head IS NULL OR length(code_head) IN (40,64)),
    full_name TEXT,
    owner TEXT,
    name TEXT,
    description TEXT,
    language TEXT,
    fork BOOLEAN NOT NULL DEFAULT FALSE,
    archived BOOLEAN NOT NULL DEFAULT FALSE,
    stars BIGINT NOT NULL DEFAULT 0,
    forks BIGINT NOT NULL DEFAULT 0,
    topics TEXT[] NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ,
    pushed_at TIMESTAMPTZ
);
CREATE INDEX search_repositories_full_name ON search_repositories(full_name);
CREATE INDEX search_repositories_owner ON search_repositories(owner);
CREATE INDEX search_repositories_stars ON search_repositories(stars DESC,id DESC) WHERE document IS NOT NULL AND public;
CREATE TABLE search_readers (
    repository_id BIGINT NOT NULL REFERENCES search_repositories(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL CHECK(user_id BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY(repository_id,user_id)
);
CREATE INDEX search_readers_user ON search_readers(user_id,repository_id);
CREATE TABLE search_issues (
    pull BOOLEAN NOT NULL,
    id BIGINT NOT NULL CHECK(id BETWEEN 1 AND 9007199254740991),
    repository_id BIGINT NOT NULL CHECK(repository_id BETWEEN 1 AND 9007199254740991),
    cell_id INTEGER NOT NULL REFERENCES directory_cells(id),
    event_id BIGINT NOT NULL,
    attempt BIGINT NOT NULL,
    document JSONB CHECK(document IS NULL OR (jsonb_typeof(document)='object' AND octet_length(document::text)<=524288)),
    state TEXT,
    merged BOOLEAN NOT NULL DEFAULT FALSE,
    draft BOOLEAN NOT NULL DEFAULT FALSE,
    locked BOOLEAN NOT NULL DEFAULT FALSE,
    author TEXT,
    labels TEXT[] NOT NULL DEFAULT '{}',
    assignees TEXT[] NOT NULL DEFAULT '{}',
    comments BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ,
    title TEXT,
    body TEXT,
    PRIMARY KEY(pull,id)
);
CREATE INDEX search_issues_repository ON search_issues(repository_id);
CREATE TABLE search_code (
    repository_id BIGINT NOT NULL REFERENCES search_repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL CHECK(octet_length(path) BETWEEN 1 AND 4096),
    sha TEXT NOT NULL CHECK(length(sha) IN (40,64)),
    size BIGINT NOT NULL CHECK(size>=0),
    content TEXT NOT NULL CHECK(octet_length(content)<=131072),
    PRIMARY KEY(repository_id,path)
);
