-- Webhooks on a repository or an organization, their encrypted secrets, and recent deliveries.
CREATE TABLE webhooks (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE CHECK (org_id IS NULL OR typeof(org_id)='integer'),
    payload_url TEXT NOT NULL CHECK (length(payload_url) <= 2048),
    content_type TEXT NOT NULL CHECK (content_type IN ('application/json','application/x-www-form-urlencoded')),
    encrypted_secret BLOB,
    secret_nonce BLOB,
    events TEXT NOT NULL CHECK (length(events) <= 1024),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1)),
    ssl_verify INTEGER NOT NULL DEFAULT 1 CHECK (ssl_verify IN (0,1)),
    created_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (created_by_id IS NULL OR typeof(created_by_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK ((repo_id IS NULL) <> (org_id IS NULL)),
    CHECK ((encrypted_secret IS NULL) = (secret_nonce IS NULL))
);

CREATE INDEX idx_webhooks_repo_id ON webhooks(repo_id);

CREATE INDEX idx_webhooks_org_id ON webhooks(org_id);

CREATE TABLE webhook_deliveries (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    webhook_id INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE CHECK (typeof(webhook_id)='integer'),
    guid TEXT NOT NULL CHECK (length(guid) <= 64),
    event TEXT NOT NULL CHECK (length(event) <= 64),
    action TEXT CHECK (length(action) <= 64),
    status_code INTEGER,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    error TEXT CHECK (length(error) <= 512),
    request_headers TEXT NOT NULL,
    request_body TEXT NOT NULL,
    response_headers TEXT,
    response_body TEXT,
    delivered_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE INDEX idx_webhook_deliveries_webhook_id ON webhook_deliveries(webhook_id, id);
