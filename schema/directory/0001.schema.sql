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
    read_regions JSONB NOT NULL DEFAULT '[]' CHECK(jsonb_typeof(read_regions)='array' AND jsonb_array_length(read_regions)<=16 AND octet_length(read_regions::text)<=4194304),
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
-- Which directory cluster holds each of the 256 logical shards. Only the registry database
-- reads this table; its rows are written by `Directory::migrate`. A shard being copied to
-- another cluster is `moving`: lookups still read it, writes wait or are refused. After the
-- switch the old copy stays as `moved_from` until lookups that read the old map are done.
CREATE TABLE directory_shards (
    shard SMALLINT PRIMARY KEY CHECK(shard BETWEEN 0 AND 255),
    cluster TEXT NOT NULL CHECK(cluster ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    state TEXT NOT NULL DEFAULT 'serving' CHECK(state IN ('serving','moving')),
    target TEXT CHECK(target ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    moved_from TEXT CHECK(moved_from ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    moved_at TIMESTAMPTZ,
    CHECK((state='moving')=(target IS NOT NULL) AND target IS DISTINCT FROM cluster),
    CHECK((moved_from IS NULL)=(moved_at IS NULL))
);
-- A logical shard holds its keys and its own copy of every object they belong to, so each
-- shard moves alone. An object's home row is the copy in its own shard (`Object::shard`).
-- The home cell is checked against the registry, which may be another database.
CREATE TABLE directory_objects (
    shard SMALLINT NOT NULL CHECK(shard BETWEEN 0 AND 255),
    kind TEXT NOT NULL CHECK(kind IN ('user','organization','repository')),
    id BIGINT NOT NULL CHECK(id BETWEEN 1 AND 9007199254740991),
    home_cell INTEGER NOT NULL CHECK(home_cell BETWEEN 0 AND 65535),
    active BOOLEAN NOT NULL DEFAULT FALSE,
    claim_count INTEGER NOT NULL DEFAULT 0 CHECK(claim_count BETWEEN 0 AND 1000),
    -- On the home row of a pending object: how many keys its reservation holds in all shards.
    key_count INTEGER NOT NULL DEFAULT 0 CHECK(key_count BETWEEN 0 AND 1000),
    nonce_hash TEXT NOT NULL CHECK(length(nonce_hash)=64),
    expires_at TIMESTAMPTZ,
    PRIMARY KEY(shard,kind,id),
    CHECK((active AND expires_at IS NULL) OR (NOT active AND expires_at IS NOT NULL))
);
CREATE INDEX directory_objects_object ON directory_objects(kind,id);
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
    FOREIGN KEY(shard,object_kind,object_id) REFERENCES directory_objects(shard,kind,id) ON DELETE CASCADE,
    CHECK((confirmed AND expires_at IS NULL) OR (NOT confirmed AND expires_at IS NOT NULL))
);
CREATE INDEX directory_claims_object ON directory_claims(shard,object_kind,object_id,nonce_hash);
CREATE INDEX directory_claims_expiry ON directory_claims(expires_at) WHERE NOT confirmed;

-- The parent count bounds every per-object operation, including hostile repeated batches.
CREATE FUNCTION directory_claim_count() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP IN ('DELETE','UPDATE') THEN
        UPDATE directory_objects SET claim_count=claim_count-1 WHERE shard=OLD.shard AND kind=OLD.object_kind AND id=OLD.object_id;
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        UPDATE directory_objects SET claim_count=claim_count+1 WHERE shard=NEW.shard AND kind=NEW.object_kind AND id=NEW.object_id;
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER directory_claim_count AFTER INSERT OR DELETE OR UPDATE OF shard,object_kind,object_id ON directory_claims FOR EACH ROW EXECUTE FUNCTION directory_claim_count();

-- The global search index. Each cell writes its own repositories' documents through its
-- outbox; a later (event, attempt) replaces an earlier one, and a removed repository keeps
-- an empty row so a late write cannot bring it back. Private documents are returned only to
-- the readers their cell names. Names and code match substrings; prose matches words.
-- The extension lives in `public`, not the directory's own schema, so dropping a schema
-- never drops it; if it is already installed elsewhere, that schema is searched for the
-- rest of this migration.
CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public;
SELECT set_config('search_path', current_setting('search_path') || ',' || quote_ident(n.nspname), true)
FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='pg_trgm';
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
CREATE INDEX search_repositories_full_name ON search_repositories(lower(full_name));
CREATE INDEX search_repositories_owner ON search_repositories(lower(owner));
CREATE INDEX search_repositories_name_text ON search_repositories USING gin(name gin_trgm_ops);
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
-- Ranked prose uses a fixed dictionary, independent of each connection's configuration.
-- Separate vectors keep `in:title`, `in:body` and `in:comments` indexed as well.
ALTER TABLE search_repositories ADD COLUMN description_words TSVECTOR GENERATED ALWAYS AS
    (to_tsvector('simple'::regconfig,coalesce(description,''))) STORED;
CREATE INDEX search_repositories_description_words ON search_repositories USING gin(description_words);
ALTER TABLE search_issues ADD COLUMN comment_text TEXT NOT NULL DEFAULT '';
ALTER TABLE search_issues ADD COLUMN title_words TSVECTOR GENERATED ALWAYS AS
    (setweight(to_tsvector('simple'::regconfig,coalesce(title,'')),'A')) STORED;
ALTER TABLE search_issues ADD COLUMN body_words TSVECTOR GENERATED ALWAYS AS
    (setweight(to_tsvector('simple'::regconfig,coalesce(body,'')),'B')) STORED;
ALTER TABLE search_issues ADD COLUMN comment_words TSVECTOR GENERATED ALWAYS AS
    (setweight(to_tsvector('simple'::regconfig,comment_text),'C')) STORED;
CREATE INDEX search_issues_title_words ON search_issues USING gin(title_words);
CREATE INDEX search_issues_body_words ON search_issues USING gin(body_words);
CREATE INDEX search_issues_comment_words ON search_issues USING gin(comment_words);
-- File text is stored once per distinct content, keyed by the SHA-256 the directory computes
-- itself, so a cell cannot attach other text to another repository's file.
CREATE TABLE search_blobs (
    digest BYTEA PRIMARY KEY CHECK(octet_length(digest)=32),
    content TEXT NOT NULL CHECK(octet_length(content)<=131072)
);
CREATE INDEX search_blobs_content_text ON search_blobs USING gin(content gin_trgm_ops);
CREATE TABLE search_code (
    repository_id BIGINT NOT NULL REFERENCES search_repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL CHECK(octet_length(path) BETWEEN 1 AND 4096),
    sha TEXT NOT NULL CHECK(length(sha) IN (40,64)),
    size BIGINT NOT NULL CHECK(size>=0),
    digest BYTEA NOT NULL REFERENCES search_blobs(digest),
    PRIMARY KEY(repository_id,path)
);
CREATE INDEX search_code_digest ON search_code(digest);
CREATE INDEX search_code_path_text ON search_code USING gin(path gin_trgm_ops);
-- Packages a public repository depends on, or provides (its root npm package), for "used by".
CREATE TABLE search_packages (
    repository_id BIGINT NOT NULL REFERENCES search_repositories(id) ON DELETE CASCADE,
    ecosystem TEXT NOT NULL CHECK(octet_length(ecosystem) BETWEEN 1 AND 32),
    name TEXT NOT NULL CHECK(octet_length(name) BETWEEN 1 AND 256),
    provided BOOLEAN NOT NULL,
    PRIMARY KEY(repository_id,provided,ecosystem,name)
);
CREATE INDEX search_packages_name ON search_packages(ecosystem,name,provided);

-- Regions where an organization's members read from its home cell's regional web nodes.
-- Writes still go to the one home cell. Rows appear by hand or from counted member requests;
-- forgetting the organization removes them.
CREATE TABLE directory_presence (
    organization_id BIGINT NOT NULL CHECK(organization_id BETWEEN 1 AND 9007199254740991),
    region TEXT NOT NULL CHECK(octet_length(region) BETWEEN 1 AND 63),
    manual BOOLEAN NOT NULL,
    added_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(organization_id,region)
);
