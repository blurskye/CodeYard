-- codeyard SQLite schema.
-- UUID=BLOB(16), arrays/JSON=JSON TEXT, bool=INTEGER,
-- UTC times=RFC3339 TEXT with fixed six decimal places.

CREATE TABLE files (
    id BLOB NOT NULL PRIMARY KEY DEFAULT (randomblob(16)) CHECK (id IS NULL OR (typeof(id)='blob' AND length(id)=16)),
    user_id INTEGER CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    filename TEXT NOT NULL,
    original_name TEXT NOT NULL,
    content_type TEXT NOT NULL,
    file_size INTEGER NOT NULL CHECK (file_size IS NULL OR typeof(file_size)='integer'),
    sha256_hex TEXT NOT NULL CHECK (length(sha256_hex)=64 AND sha256_hex NOT GLOB '*[^0-9a-f]*'),
    object_key TEXT NOT NULL UNIQUE,
    reason TEXT CHECK (length(reason)<=50),
    upload_expires_at TEXT,
    put_expires_at TEXT,
    confirmed_at TEXT,
    is_active INTEGER NOT NULL DEFAULT 0 CHECK (is_active IN (0,1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    visibility TEXT NOT NULL DEFAULT 'owner',
    FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
);

-- Object keys whose stored bytes can be deleted once delete_after has passed.
CREATE TABLE pending_object_deletions (
    object_key TEXT PRIMARY KEY,
    delete_after TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE INDEX idx_pending_object_deletions_due
    ON pending_object_deletions(delete_after);

CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    username TEXT NOT NULL UNIQUE CHECK (length(username)<=255),
    email TEXT UNIQUE CHECK (length(email)<=255),
    password_hash TEXT CHECK (length(password_hash)<=255),
    avatar_file_id BLOB REFERENCES files(id) ON DELETE SET NULL CHECK (avatar_file_id IS NULL OR (typeof(avatar_file_id)='blob' AND length(avatar_file_id)=16)),
    display_name TEXT CHECK (length(display_name)<=255),
    bio TEXT,
    location TEXT CHECK (length(location)<=255),
    website TEXT CHECK (length(website)<=255),
    is_admin INTEGER NOT NULL DEFAULT 0 CHECK (is_admin IN (0,1)),
    is_banned INTEGER NOT NULL DEFAULT 0 CHECK (is_banned IN (0,1)),
    must_change_password INTEGER NOT NULL DEFAULT 0 CHECK (must_change_password IN (0,1)),
    email_verified INTEGER NOT NULL DEFAULT 0 CHECK (email_verified IN (0,1)),
    two_factor_enabled INTEGER NOT NULL DEFAULT 0 CHECK (two_factor_enabled IN (0,1)),
    -- Whether notifications are also sent by email, when the site has a mail server.
    email_notifications INTEGER NOT NULL DEFAULT 1 CHECK (email_notifications IN (0,1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE UNIQUE INDEX users_username_folded ON users(lower(username));

CREATE TABLE node_configuration (
    id INTEGER PRIMARY KEY CHECK (id IS NULL OR typeof(id)='integer') CHECK (id BETWEEN -32768 AND 32767),
    node_mode TEXT NOT NULL DEFAULT 'public' CHECK (length(node_mode)<=20),
    setup_completed INTEGER NOT NULL DEFAULT 0 CHECK (setup_completed IN (0,1)),
    -- '1' keeps one copy (the default), a count such as '2' keeps that many copies in
    -- all, and 'all' keeps a copy on every storage node.
    replicas TEXT NOT NULL DEFAULT '1'
        CHECK (replicas = 'all' OR (replicas GLOB '[1-9]*' AND replicas NOT GLOB '*[^0-9]*' AND length(replicas) <= 3)),
    -- Whether robots.txt and ai.txt block AI training crawlers, and whether HTML pages say so.
    -- Blocked until an administrator allows them.
    ai_crawlers TEXT NOT NULL DEFAULT 'block' CHECK (ai_crawlers IN ('allow', 'block')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    -- Single-node mode allows only the server and runners; `codeyard cluster enable` turns on
    -- cluster mode.
    cluster_enabled INTEGER NOT NULL DEFAULT 0 CHECK (cluster_enabled IN (0,1)),
    CONSTRAINT node_configuration_singleton CHECK (id = 1),
    CONSTRAINT node_configuration_mode_check CHECK (node_mode IN ('public', 'private'))
);

CREATE TABLE cluster_ssh_host_key (
    id INTEGER PRIMARY KEY CHECK (id IS NULL OR typeof(id)='integer') CHECK (id BETWEEN -32768 AND 32767),
    private_key_pem TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CONSTRAINT cluster_ssh_host_key_singleton CHECK (id = 1)
);

CREATE TABLE signup_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    username TEXT NOT NULL CHECK (length(username)<=255),
    username_key TEXT NOT NULL,
    email_key TEXT,
    email TEXT CHECK (length(email)<=255),
    password_hash TEXT NOT NULL CHECK (length(password_hash)<=255),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (length(status)<=20), 
    reviewed_by INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (reviewed_by IS NULL OR typeof(reviewed_by)='integer'),
    reviewed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CONSTRAINT signup_requests_status_check CHECK (status IN ('pending', 'approved', 'rejected'))
);

CREATE TABLE oauth_accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    provider TEXT NOT NULL CHECK (length(provider)<=50),
    provider_user_id TEXT NOT NULL CHECK (length(provider_user_id)<=255),
    access_token TEXT,
    refresh_token TEXT,
    token_expires_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(provider, provider_user_id)
);

CREATE TABLE sessions (
    ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE CHECK (length(id)<=64),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    user_agent TEXT,
    ip_address TEXT,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    csrf_token TEXT CHECK (length(csrf_token)<=64),
    csrf_expires_at TEXT
);

CREATE TABLE anonymous_sessions (
    id TEXT NOT NULL PRIMARY KEY CHECK (length(id)<=64),
    user_agent TEXT,
    ip_address TEXT,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    csrf_token TEXT CHECK (length(csrf_token)<=64),
    csrf_expires_at TEXT
);

CREATE TABLE oauth_states (
    state TEXT NOT NULL PRIMARY KEY CHECK (length(state)<=64),
    pkce_verifier TEXT NOT NULL CHECK (length(pkce_verifier)<=128),
    provider TEXT NOT NULL CHECK (length(provider)<=50),
    redirect_uri TEXT,
    return_to TEXT NOT NULL DEFAULT '/',
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    browser_binding TEXT CHECK (length(browser_binding)<=64)
);

CREATE TABLE personal_access_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
    scope TEXT NOT NULL CHECK (scope IN ('read', 'write')),
    expires_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    last_used_at TEXT,
    last_used_ip TEXT CHECK (last_used_ip IS NULL OR length(last_used_ip) <= 64)
);
CREATE INDEX idx_personal_access_tokens_user ON personal_access_tokens(user_id);

CREATE TABLE ssh_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=255),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint)<=255),
    public_key TEXT NOT NULL,
    key_type TEXT NOT NULL CHECK (length(key_type)<=50),
    last_used_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CONSTRAINT ssh_keys_fingerprint_unique UNIQUE (fingerprint)
);

-- Successful signed-byte verifications survive enrollment/key deletion. No signed content is stored.
CREATE TABLE signature_verifications (
    input_hash TEXT PRIMARY KEY NOT NULL CHECK (length(input_hash) = 64),
    verified_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE gpg_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    primary_key_id INTEGER REFERENCES gpg_keys(id) ON DELETE CASCADE,
    name TEXT CHECK (length(name)<=100),
    fingerprint TEXT NOT NULL UNIQUE,
    key_id TEXT NOT NULL,
    key_data TEXT NOT NULL CHECK (length(key_data)<=262144)
);

-- The encrypted TOTP secret, the last accepted time step (a code works once) and the hashed
-- one-time recovery codes. users.two_factor_enabled is kept in step.
CREATE TABLE two_factor_auth (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    totp_secret_encrypted BLOB NOT NULL,
    totp_nonce BLOB NOT NULL,
    last_used_step INTEGER CHECK (last_used_step IS NULL OR typeof(last_used_step)='integer'),
    enabled_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE two_factor_recovery_codes (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    code_hash TEXT NOT NULL CHECK (length(code_hash)=64),
    PRIMARY KEY (user_id, code_hash)
);

CREATE TABLE organizations (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    name TEXT NOT NULL UNIQUE CHECK (length(name)<=255),
    display_name TEXT CHECK (length(display_name)<=255),
    description TEXT,
    avatar_file_id BLOB REFERENCES files(id) ON DELETE SET NULL CHECK (avatar_file_id IS NULL OR (typeof(avatar_file_id)='blob' AND length(avatar_file_id)=16)),
    billing_email TEXT CHECK (length(billing_email)<=255),
    is_verified INTEGER NOT NULL DEFAULT 0 CHECK (is_verified IN (0,1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE org_memberships (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK (org_id IS NULL OR typeof(org_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    role TEXT NOT NULL DEFAULT 'member' CHECK (length(role)<=20), 
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    is_public INTEGER NOT NULL DEFAULT 0 CHECK (is_public IN (0,1)),
    UNIQUE(org_id, user_id)
);

-- Organization invitations: a pending offer of membership. Membership starts only when the
-- invitee accepts; a decline or a cancellation deletes the row.
CREATE TABLE organization_invitations (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK (typeof(org_id)='integer'),
    user_id INTEGER REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    role TEXT NOT NULL DEFAULT 'member' CHECK (length(role)<=20),
    invited_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (invited_by_id IS NULL OR typeof(invited_by_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    email TEXT CHECK (length(email)<=255),
    expires_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now','+7 days')),
    failed_at TEXT,
    failed_reason TEXT,
    CHECK (user_id IS NOT NULL OR email IS NOT NULL),
    UNIQUE(org_id, user_id)
);

CREATE INDEX idx_organization_invitations_user_id ON organization_invitations(user_id);

CREATE TABLE teams (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK (org_id IS NULL OR typeof(org_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=255),
    slug TEXT NOT NULL CHECK (length(slug)<=255),
    description TEXT,
    privacy TEXT NOT NULL DEFAULT 'visible' CHECK (length(privacy)<=20), 
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    default_permission INTEGER NOT NULL DEFAULT 1 CHECK (default_permission BETWEEN 1 AND 5),
    notifications_enabled INTEGER NOT NULL DEFAULT 1 CHECK (notifications_enabled IN (0,1)),
    parent_team_id INTEGER REFERENCES teams(id) ON DELETE SET NULL CHECK (parent_team_id IS NULL OR parent_team_id<>id),
    UNIQUE(org_id, slug)
);

CREATE TABLE team_memberships (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE CHECK (team_id IS NULL OR typeof(team_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    role TEXT NOT NULL DEFAULT 'member' CHECK (length(role)<=20), 
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(team_id, user_id)
);

CREATE TABLE repositories (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    owner_kind TEXT NOT NULL DEFAULT 'user' CHECK (length(owner_kind)<=10), 
    owner_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE CHECK (owner_user_id IS NULL OR typeof(owner_user_id)='integer'),
    owner_org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE CHECK (owner_org_id IS NULL OR typeof(owner_org_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=255),
    description TEXT,
    website TEXT CHECK (length(website)<=255),
    is_private INTEGER NOT NULL DEFAULT 0 CHECK (is_private IN (0,1)),
    is_archived INTEGER NOT NULL DEFAULT 0 CHECK (is_archived IN (0,1)),
    is_fork INTEGER NOT NULL DEFAULT 0 CHECK (is_fork IN (0,1)),
    forked_from_id INTEGER REFERENCES repositories(id) ON DELETE SET NULL CHECK (forked_from_id IS NULL OR typeof(forked_from_id)='integer'),
    -- Canonical Git refs can contain 1024 bytes.
    default_branch TEXT NOT NULL DEFAULT 'main' CHECK (length(default_branch)<=1024),
    milestone_number INTEGER NOT NULL DEFAULT 0 CHECK (milestone_number BETWEEN 0 AND 2147483647),
    allow_merge_commit INTEGER NOT NULL DEFAULT 1 CHECK (allow_merge_commit IN (0,1)),
    allow_squash_merge INTEGER NOT NULL DEFAULT 1 CHECK (allow_squash_merge IN (0,1)),
    allow_rebase_merge INTEGER NOT NULL DEFAULT 1 CHECK (allow_rebase_merge IN (0,1)),
    disk_path TEXT NOT NULL,
    storage_node_id INTEGER REFERENCES nodes(id),
    size_bytes INTEGER NOT NULL DEFAULT 0 CHECK (size_bytes IS NULL OR typeof(size_bytes)='integer'),
    stars_count INTEGER NOT NULL DEFAULT 0 CHECK (stars_count IS NULL OR typeof(stars_count)='integer') CHECK (stars_count BETWEEN -2147483648 AND 2147483647),
    forks_count INTEGER NOT NULL DEFAULT 0 CHECK (forks_count IS NULL OR typeof(forks_count)='integer') CHECK (forks_count BETWEEN -2147483648 AND 2147483647),
    watchers_count INTEGER NOT NULL DEFAULT 0 CHECK (watchers_count IS NULL OR typeof(watchers_count)='integer') CHECK (watchers_count BETWEEN -2147483648 AND 2147483647),
    open_issues_count INTEGER NOT NULL DEFAULT 0 CHECK (open_issues_count IS NULL OR typeof(open_issues_count)='integer') CHECK (open_issues_count BETWEEN -2147483648 AND 2147483647),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    visibility_locked INTEGER NOT NULL DEFAULT 0 CHECK (visibility_locked IN (0,1)),
    -- Counts the ref changes made on the primary. A replica is current when its
    -- applied_version equals this.
    write_version INTEGER NOT NULL DEFAULT 0 CHECK (write_version >= 0),
    -- The user who created the repository, so an organization repository is listed under
    -- its creator. No foreign key: a deleted user's id matches no one. Repositories with no
    -- recorded creator fall back to their owner.
    created_by_id INTEGER CHECK (created_by_id IS NULL OR typeof(created_by_id)='integer'),
    is_gist INTEGER NOT NULL DEFAULT 0 CHECK (is_gist IN (0,1)),
    contribution_version INTEGER NOT NULL DEFAULT 0,
    actions_engine TEXT CHECK (actions_engine IN ('codeyard-actions','act')),
    is_template INTEGER NOT NULL DEFAULT 0 CHECK (is_template IN (0,1)),
    -- GitHub Enterprise internal visibility. Access stays private: members and collaborators only.
    is_internal INTEGER NOT NULL DEFAULT 0 CHECK (is_internal IN (0,1)),
    CONSTRAINT repo_internal_private CHECK (NOT is_internal OR is_private),
    CONSTRAINT repo_owner_check CHECK (
        (owner_kind = 'user' AND owner_user_id IS NOT NULL AND owner_org_id IS NULL) OR
        (owner_kind = 'org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL)
    ),
    CONSTRAINT repo_unique_user UNIQUE (owner_user_id, name),
    CONSTRAINT repo_unique_org UNIQUE (owner_org_id, name)
);

CREATE TABLE repo_access (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    access_level INTEGER NOT NULL DEFAULT 1 CHECK (access_level IS NULL OR typeof(access_level)='integer') CHECK (access_level BETWEEN -32768 AND 32767), 
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(user_id, repo_id)
);

CREATE TABLE team_repo_access (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE CHECK (team_id IS NULL OR typeof(team_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    permission INTEGER NOT NULL DEFAULT 1 CHECK (permission IS NULL OR typeof(permission)='integer') CHECK (permission BETWEEN -32768 AND 32767), 
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(team_id, repo_id)
);

CREATE TABLE deploy_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=255),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint)<=255),
    public_key TEXT NOT NULL,
    read_only INTEGER NOT NULL DEFAULT 1 CHECK (read_only IN (0,1)),
    last_used_at TEXT,
    -- The account that added the key, which its pushes are attributed to.
    created_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (created_by_id IS NULL OR typeof(created_by_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(repo_id, fingerprint)
);

CREATE TABLE repo_languages (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    language TEXT NOT NULL CHECK (length(language)<=100),
    bytes INTEGER NOT NULL CHECK (bytes IS NULL OR typeof(bytes)='integer'),
    percentage REAL NOT NULL CHECK (typeof(percentage) IN ('integer','real') AND percentage BETWEEN -999.99 AND 999.99),
    color TEXT CHECK (length(color)<=7),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(repo_id, language)
);

-- Topics group repositories on Explore. Writes keep at most 20 per repository, a count that a
-- constraint cannot check.
CREATE TABLE repository_topics (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    topic TEXT NOT NULL CHECK (length(topic) BETWEEN 1 AND 35 AND topic NOT GLOB '*[^a-z0-9-]*'),
    PRIMARY KEY (repo_id, topic)
);
CREATE INDEX idx_repository_topics_topic ON repository_topics(topic);

CREATE TABLE repo_storage_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    size_bytes INTEGER NOT NULL CHECK (size_bytes IS NULL OR typeof(size_bytes)='integer'),
    recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

-- Branch updates made by pushes, the web editor, the API and merges, for profile activity.
-- Each row is one branch update. Commit counts come from Git's own range count.
CREATE TABLE repository_pushes (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    actor_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (actor_id IS NULL OR typeof(actor_id)='integer'),
    branch TEXT NOT NULL CHECK (length(branch)<=1024),
    commit_count INTEGER NOT NULL DEFAULT 0 CHECK (typeof(commit_count)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE branch_protection_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    pattern TEXT NOT NULL CHECK (length(pattern)<=255), 
    require_pull_request INTEGER NOT NULL DEFAULT 0 CHECK (require_pull_request IN (0,1)),
    required_approving_reviews INTEGER NOT NULL DEFAULT 0 CHECK (required_approving_reviews IS NULL OR typeof(required_approving_reviews)='integer') CHECK (required_approving_reviews BETWEEN -2147483648 AND 2147483647),
    dismiss_stale_reviews INTEGER NOT NULL DEFAULT 0 CHECK (dismiss_stale_reviews IN (0,1)),
    require_code_owner_review INTEGER NOT NULL DEFAULT 0 CHECK (require_code_owner_review IN (0,1)),
    require_status_checks INTEGER NOT NULL DEFAULT 0 CHECK (require_status_checks IN (0,1)),
    required_status_checks TEXT DEFAULT '[]' CHECK (required_status_checks IS NULL OR (json_valid(required_status_checks) AND json_type(required_status_checks)='array')),
    require_up_to_date_branch INTEGER NOT NULL DEFAULT 0 CHECK (require_up_to_date_branch IN (0,1)),
    require_linear_history INTEGER NOT NULL DEFAULT 0 CHECK (require_linear_history IN (0,1)),
    block_force_pushes INTEGER NOT NULL DEFAULT 1 CHECK (block_force_pushes IN (0,1)),
    block_deletions INTEGER NOT NULL DEFAULT 1 CHECK (block_deletions IN (0,1)),
    restrict_pushes INTEGER NOT NULL DEFAULT 0 CHECK (restrict_pushes IN (0,1)),
    push_allowances TEXT DEFAULT '[]' CHECK (push_allowances IS NULL OR (json_valid(push_allowances) AND json_type(push_allowances)='array')),
    -- Users who may force push to a branch whose rule blocks force pushes. Like push_allowances,
    -- it is a JSON array of user ids.
    force_push_allowances TEXT DEFAULT '[]'
        CHECK (force_push_allowances IS NULL OR (json_valid(force_push_allowances) AND json_type(force_push_allowances)='array')),
    enforce_admins INTEGER NOT NULL DEFAULT 1 CHECK (enforce_admins IN (0,1)),
    require_conversation_resolution INTEGER NOT NULL DEFAULT 0 CHECK (require_conversation_resolution IN (0,1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    -- Team allowances restrict existing writers; they never grant repository access.
    push_team_allowances TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(push_team_allowances) AND json_type(push_team_allowances)='array'),
    github_settings TEXT NOT NULL DEFAULT '{}',
    UNIQUE(repo_id, pattern)
);

CREATE TABLE commit_statuses (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    sha TEXT NOT NULL CHECK (length(sha)<=40),
    context TEXT NOT NULL,
    state TEXT NOT NULL CHECK (length(state)<=20),
    description TEXT,
    target_url TEXT,
    creator_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (creator_id IS NULL OR typeof(creator_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    ci_run_id INTEGER REFERENCES ci_runs(id) ON DELETE SET NULL CHECK (ci_run_id IS NULL OR typeof(ci_run_id)='integer'),
    UNIQUE(repo_id, sha, context)
);

CREATE TABLE labels (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=50),
    color TEXT NOT NULL DEFAULT '#6b7280' CHECK (length(color)<=7),
    description TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT,
    UNIQUE(repo_id, name)
);

CREATE TABLE milestones (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    number INTEGER NOT NULL CHECK (number IS NULL OR typeof(number)='integer') CHECK (number BETWEEN -2147483648 AND 2147483647),
    title TEXT NOT NULL CHECK (length(title)<=255),
    description TEXT,
    due_date TEXT,
    state TEXT NOT NULL DEFAULT 'open' CHECK (length(state)<=10),
    open_issues INTEGER NOT NULL DEFAULT 0 CHECK (open_issues IS NULL OR typeof(open_issues)='integer') CHECK (open_issues BETWEEN -2147483648 AND 2147483647),
    closed_issues INTEGER NOT NULL DEFAULT 0 CHECK (closed_issues IS NULL OR typeof(closed_issues)='integer') CHECK (closed_issues BETWEEN -2147483648 AND 2147483647),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    closed_at TEXT,
    creator_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (creator_id IS NULL OR typeof(creator_id)='integer'),
    updated_at TEXT,
    due_on TEXT,
    UNIQUE(repo_id, number)
);

CREATE TABLE issues (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    number INTEGER NOT NULL CHECK (number IS NULL OR typeof(number)='integer') CHECK (number BETWEEN -2147483648 AND 2147483647),
    author_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (author_id IS NULL OR typeof(author_id)='integer'),
    title TEXT NOT NULL CHECK (length(title)<=255),
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'open' CHECK (length(state)<=10),
    state_reason TEXT CHECK (length(state_reason)<=20), 
    locked INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0,1)),
    lock_reason TEXT CHECK (length(lock_reason)<=20),
    milestone_id INTEGER REFERENCES milestones(id) ON DELETE SET NULL CHECK (milestone_id IS NULL OR typeof(milestone_id)='integer'),
    closed_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (closed_by_id IS NULL OR typeof(closed_by_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    closed_at TEXT,
    UNIQUE(repo_id, number)
);

CREATE TABLE issue_labels (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE CHECK (issue_id IS NULL OR typeof(issue_id)='integer'),
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE CHECK (label_id IS NULL OR typeof(label_id)='integer'),
    PRIMARY KEY (issue_id, label_id)
);

CREATE TABLE issue_assignees (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE CHECK (issue_id IS NULL OR typeof(issue_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    PRIMARY KEY (issue_id, user_id)
);

CREATE TABLE issue_comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE CHECK (issue_id IS NULL OR typeof(issue_id)='integer'),
    author_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (author_id IS NULL OR typeof(author_id)='integer'),
    body TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

-- What changed on an issue or pull request, shown between its comments.
CREATE TABLE timeline_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('issue','pull_request')),
    subject_id INTEGER NOT NULL CHECK (typeof(subject_id)='integer'),
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (actor_id IS NULL OR typeof(actor_id)='integer'),
    kind TEXT NOT NULL CHECK (length(kind)<=20),
    detail TEXT NOT NULL DEFAULT '' CHECK (length(detail)<=255),
    previous TEXT NOT NULL DEFAULT '' CHECK (length(previous)<=255),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX idx_timeline_events_subject ON timeline_events(subject_kind, subject_id, created_at);
CREATE INDEX idx_timeline_events_repo_id ON timeline_events(repo_id, created_at, id);

CREATE TABLE reactions (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (typeof(repo_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('issue','issue_comment','pull_request','pr_comment','pr_review','release','commit_comment')),
    subject_id INTEGER NOT NULL CHECK (typeof(subject_id)='integer'),
    content TEXT NOT NULL CHECK (content IN ('+1','-1','laugh','hooray','confused','heart','rocket','eyes')) CHECK (subject_kind<>'release' OR content NOT IN ('-1','confused')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(user_id,subject_kind,subject_id,content)
);
CREATE INDEX idx_reactions_subject ON reactions(subject_kind,subject_id);
CREATE INDEX idx_reactions_repo_id ON reactions(repo_id);

CREATE TABLE pull_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    number INTEGER NOT NULL CHECK (number IS NULL OR typeof(number)='integer') CHECK (number BETWEEN -2147483648 AND 2147483647),
    author_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (author_id IS NULL OR typeof(author_id)='integer'),
    title TEXT NOT NULL CHECK (length(title)<=255),
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'open' CHECK (length(state)<=10),
    head_repo_id INTEGER REFERENCES repositories(id) ON DELETE SET NULL CHECK (head_repo_id IS NULL OR typeof(head_repo_id)='integer'),
    workflows_approved_sha TEXT CHECK (length(workflows_approved_sha)<=40),
    head_branch TEXT NOT NULL CHECK (length(head_branch)<=255),
    head_sha TEXT NOT NULL CHECK (length(head_sha)<=40),
    base_branch TEXT NOT NULL CHECK (length(base_branch)<=255),
    base_sha TEXT NOT NULL CHECK (length(base_sha)<=40),
    -- The base a stacked layer was built on, kept while its base branch moves on. NULL means base_sha is that base.
    fork_sha TEXT CHECK (length(fork_sha)<=40),
    is_draft INTEGER NOT NULL DEFAULT 0 CHECK (is_draft IN (0,1)),
    mergeable INTEGER CHECK (mergeable IN (0,1)),
    merged INTEGER NOT NULL DEFAULT 0 CHECK (merged IN (0,1)),
    merged_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (merged_by_id IS NULL OR typeof(merged_by_id)='integer'),
    closed_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (closed_by_id IS NULL OR typeof(closed_by_id)='integer'),
    merge_commit_sha TEXT CHECK (length(merge_commit_sha)<=40),
    stack_id INTEGER CHECK (stack_id IS NULL OR typeof(stack_id)='integer'),
    -- Counts the changes to this row's place in its stack (joined, left, closed, reopened, retargeted, merged). A rebase of the stack refuses to publish when any of them moved.
    stack_version INTEGER NOT NULL DEFAULT 0 CHECK (typeof(stack_version)='integer'),
    milestone_id INTEGER REFERENCES milestones(id) ON DELETE SET NULL CHECK (milestone_id IS NULL OR typeof(milestone_id)='integer'),
    additions INTEGER NOT NULL DEFAULT 0 CHECK (additions IS NULL OR typeof(additions)='integer') CHECK (additions BETWEEN -2147483648 AND 2147483647),
    deletions INTEGER NOT NULL DEFAULT 0 CHECK (deletions IS NULL OR typeof(deletions)='integer') CHECK (deletions BETWEEN -2147483648 AND 2147483647),
    changed_files INTEGER NOT NULL DEFAULT 0 CHECK (changed_files IS NULL OR typeof(changed_files)='integer') CHECK (changed_files BETWEEN -2147483648 AND 2147483647),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    merged_at TEXT,
    closed_at TEXT,
    locked INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0,1)),
    lock_reason TEXT CHECK (lock_reason IN ('off-topic','too heated','resolved','spam')),
    maintainer_can_modify INTEGER NOT NULL DEFAULT 0 CHECK(maintainer_can_modify IN (0,1)),
    maintainer_edit_granted_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    source_issue_id INTEGER CHECK(source_issue_id IS NULL OR (typeof(source_issue_id)='integer' AND source_issue_id>0)),
    head_publisher_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK(head_publisher_id IS NULL OR typeof(head_publisher_id)='integer'),
    UNIQUE(repo_id, number)
);

CREATE TABLE pr_labels (
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (pull_request_id IS NULL OR typeof(pull_request_id)='integer'),
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE CHECK (label_id IS NULL OR typeof(label_id)='integer'),
    PRIMARY KEY (pull_request_id, label_id)
);

CREATE TABLE pr_assignees (
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (pull_request_id IS NULL OR typeof(pull_request_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    PRIMARY KEY (pull_request_id, user_id)
);

CREATE TABLE pr_reviewers (
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (pull_request_id IS NULL OR typeof(pull_request_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    as_code_owner INTEGER NOT NULL DEFAULT 0 CHECK (as_code_owner IN (0,1)),
    PRIMARY KEY (pull_request_id, user_id)
);

CREATE TABLE pr_comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (pull_request_id IS NULL OR typeof(pull_request_id)='integer'),
    author_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (author_id IS NULL OR typeof(author_id)='integer'),
    body TEXT NOT NULL,
    path TEXT,
    line INTEGER CHECK (line IS NULL OR typeof(line)='integer') CHECK (line BETWEEN -2147483648 AND 2147483647),
    side TEXT CHECK (length(side)<=5), 
    commit_sha TEXT CHECK (length(commit_sha)<=40),
    diff_base_sha TEXT CHECK (length(diff_base_sha)=40),
    in_reply_to_comment_id INTEGER REFERENCES pr_comments(id) ON DELETE CASCADE CHECK (in_reply_to_comment_id IS NULL OR typeof(in_reply_to_comment_id)='integer'),
    resolved_at TEXT,
    resolved_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (resolved_by_id IS NULL OR typeof(resolved_by_id)='integer'),
    -- The review this comment was filed in. A pending review's comments stay private to its author.
    review_id INTEGER REFERENCES pr_reviews(id) ON DELETE CASCADE CHECK (review_id IS NULL OR typeof(review_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    start_line INTEGER CHECK(start_line>0),
    start_side TEXT CHECK((start_line IS NULL AND start_side IS NULL) OR (start_line IS NOT NULL AND start_side IS NOT NULL AND start_side IN ('LEFT','RIGHT') AND path IS NOT NULL AND line IS NOT NULL AND line>0 AND side IS NOT NULL AND side IN ('LEFT','RIGHT'))),
    source_issue_comment_id INTEGER CHECK(source_issue_comment_id IS NULL OR (typeof(source_issue_comment_id)='integer' AND source_issue_comment_id>0))
);

CREATE TABLE pr_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (pull_request_id IS NULL OR typeof(pull_request_id)='integer'),
    author_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (author_id IS NULL OR typeof(author_id)='integer'),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (length(state)<=20), 
    body TEXT,
    commit_sha TEXT CHECK (length(commit_sha)<=40),
    dismissed_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (dismissed_by_id IS NULL OR typeof(dismissed_by_id)='integer'),
    dismissed_reason TEXT,
    dismissed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

-- Files a reader marked as viewed on a pull request. The mark keeps the file's blob id, so a
-- later push that changes the file leaves it unviewed.
CREATE TABLE pull_request_viewed_files (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (typeof(pull_request_id)='integer'),
    path TEXT NOT NULL,
    blob_sha TEXT NOT NULL,
    PRIMARY KEY (user_id, pull_request_id, path)
);

CREATE TABLE releases (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    tag_name TEXT NOT NULL CHECK (length(tag_name)<=255),
    target_commitish TEXT NOT NULL DEFAULT 'main' CHECK (length(target_commitish)<=255),
    name TEXT CHECK (length(name)<=255),
    body TEXT,
    is_draft INTEGER NOT NULL DEFAULT 0 CHECK (is_draft IN (0,1)),
    is_prerelease INTEGER NOT NULL DEFAULT 0 CHECK (is_prerelease IN (0,1)),
    is_latest INTEGER NOT NULL DEFAULT 0 CHECK (is_latest IN (0,1)),
    author_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (author_id IS NULL OR typeof(author_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    published_at TEXT,
    is_immutable INTEGER NOT NULL DEFAULT 0 CHECK(is_immutable IN (0,1)),
    UNIQUE(repo_id, tag_name)
);

-- One latest release per repository.
CREATE UNIQUE INDEX idx_releases_one_latest ON releases(repo_id) WHERE is_latest = 1;

CREATE TABLE release_assets (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    release_id INTEGER NOT NULL REFERENCES releases(id) ON DELETE CASCADE CHECK (release_id IS NULL OR typeof(release_id)='integer'),
    file_id BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE CHECK (file_id IS NULL OR (typeof(file_id)='blob' AND length(file_id)=16)),
    name TEXT NOT NULL CHECK (length(name)<=255),
    label TEXT CHECK (length(label)<=255),
    content_type TEXT NOT NULL CHECK (length(content_type)<=100),
    size INTEGER NOT NULL CHECK (size IS NULL OR typeof(size)='integer'),
    download_count INTEGER NOT NULL DEFAULT 0 CHECK (download_count IS NULL OR typeof(download_count)='integer') CHECK (download_count BETWEEN -2147483648 AND 2147483647),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE UNIQUE INDEX idx_release_assets_name ON release_assets(release_id, name);

CREATE TABLE stars (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY (user_id, repo_id)
);

CREATE TABLE watches (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    level TEXT NOT NULL DEFAULT 'watching' CHECK (length(level)<=20), -- watching, custom, ignoring
    -- Comma-separated events a custom watch wants: issues, pulls.
    events TEXT NOT NULL DEFAULT '' CHECK (length(events)<=40),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY (user_id, repo_id)
);

CREATE TABLE follows (
    follower_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (follower_id IS NULL OR typeof(follower_id)='integer'),
    following_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (following_id IS NULL OR typeof(following_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY (follower_id, following_id),
    CHECK (follower_id != following_id)
);

CREATE TABLE repo_secrets (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=255),
    encrypted_value BLOB NOT NULL,
    nonce BLOB NOT NULL,
    created_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (created_by_id IS NULL OR typeof(created_by_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(repo_id, name)
);

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE CHECK (org_id IS NULL OR typeof(org_id)='integer'),
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (actor_id IS NULL OR typeof(actor_id)='integer'),
    action TEXT NOT NULL CHECK (length(action)<=100),
    target_type TEXT CHECK (length(target_type)<=50),
    target_id INTEGER CHECK (target_id IS NULL OR typeof(target_id)='integer'),
    metadata TEXT DEFAULT '{}' CHECK (metadata IS NULL OR json_valid(metadata)),
    ip_address TEXT,
    user_agent TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE upload_rate_limit (
    id BLOB NOT NULL PRIMARY KEY DEFAULT (randomblob(16)) CHECK (id IS NULL OR (typeof(id)='blob' AND length(id)=16)),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    file_id BLOB REFERENCES files(id) ON DELETE SET NULL CHECK (file_id IS NULL OR (typeof(file_id)='blob' AND length(file_id)=16)),
    file_size INTEGER NOT NULL CHECK (file_size IS NULL OR typeof(file_size)='integer'),
    reason TEXT CHECK (length(reason)<=50),
    ip_address TEXT,
    confirmed_at TEXT,
    uploaded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE INDEX idx_upload_rate_limit_confirmed ON upload_rate_limit(confirmed_at) WHERE confirmed_at IS NOT NULL;
CREATE TRIGGER record_upload_confirmation AFTER UPDATE OF confirmed_at ON files
WHEN OLD.confirmed_at IS NULL AND NEW.confirmed_at IS NOT NULL
BEGIN
    UPDATE upload_rate_limit SET confirmed_at=NEW.confirmed_at WHERE file_id=NEW.id AND confirmed_at IS NULL;
END;

CREATE TABLE download_rate_limit (
    id BLOB NOT NULL PRIMARY KEY DEFAULT (randomblob(16)) CHECK (id IS NULL OR (typeof(id)='blob' AND length(id)=16)),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    file_id BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE CHECK (file_id IS NULL OR (typeof(file_id)='blob' AND length(file_id)=16)),
    reason TEXT CHECK (length(reason)<=50),
    ip_address TEXT,
    downloaded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE notifications (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (actor_id IS NULL OR typeof(actor_id)='integer'),
    repo_id INTEGER REFERENCES repositories(id) ON DELETE SET NULL CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    kind TEXT NOT NULL CHECK (length(kind)<=64),
    title TEXT NOT NULL,
    body TEXT,
    link TEXT,
    metadata TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata)),
    read_at TEXT,
    -- Saved for later, and done (out of the inbox). NULL means not set.
    saved_at TEXT,
    done_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    -- Repository the notification was about, kept after the repository is gone
    -- so its notifications stay hidden; 0 = not about a repository.
    scope_repository_id INTEGER NOT NULL CHECK (scope_repository_id >= 0),
    thread_id INTEGER REFERENCES notification_threads(id) ON DELETE SET NULL
);

CREATE TABLE file_reason_limits (
    reason TEXT NOT NULL PRIMARY KEY CHECK (length(reason)<=50),
    max_file_size_bytes INTEGER NOT NULL DEFAULT 10485760 CHECK (max_file_size_bytes IS NULL OR typeof(max_file_size_bytes)='integer'), 
    max_uploads_per_hour INT NOT NULL DEFAULT 10 CHECK (max_uploads_per_hour IS NULL OR typeof(max_uploads_per_hour)='integer') CHECK (max_uploads_per_hour BETWEEN -2147483648 AND 2147483647),
    max_bytes_per_hour INTEGER NOT NULL DEFAULT 104857600 CHECK (max_bytes_per_hour IS NULL OR typeof(max_bytes_per_hour)='integer'), 
    max_uploads_per_day INT NOT NULL DEFAULT 50 CHECK (max_uploads_per_day IS NULL OR typeof(max_uploads_per_day)='integer') CHECK (max_uploads_per_day BETWEEN -2147483648 AND 2147483647),
    max_bytes_per_day INTEGER NOT NULL DEFAULT 524288000 CHECK (max_bytes_per_day IS NULL OR typeof(max_bytes_per_day)='integer'), 
    allowed_content_types TEXT CHECK (allowed_content_types IS NULL OR (json_valid(allowed_content_types) AND json_type(allowed_content_types)='array')), 
    requires_auth INTEGER NOT NULL DEFAULT 1 CHECK (requires_auth IN (0,1)),
    description TEXT
);

CREATE TABLE ci_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    run_id BLOB NOT NULL UNIQUE CHECK (run_id IS NULL OR (typeof(run_id)='blob' AND length(run_id)=16)),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    triggered_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (triggered_by_id IS NULL OR typeof(triggered_by_id)='integer'),
    pull_request_id INTEGER REFERENCES pull_requests(id) ON DELETE SET NULL CHECK (pull_request_id IS NULL OR typeof(pull_request_id)='integer'),
    mode TEXT NOT NULL CHECK (length(mode)<=20),
    pipeline TEXT NOT NULL DEFAULT 'ci' CHECK (length(pipeline)<=20), 
    event TEXT NOT NULL CHECK (length(event)<=50),
    is_fork INTEGER NOT NULL DEFAULT 0 CHECK (is_fork IN (0,1)),
    branch TEXT NOT NULL CHECK (length(branch)<=1024),
    head_sha TEXT CHECK (length(head_sha)<=40),
    workflow_path TEXT NOT NULL,
    environment TEXT CHECK (length(environment)<=255),
    status TEXT NOT NULL DEFAULT 'queued' CHECK (length(status)<=20), 
    error_message TEXT,
    started_at TEXT,
    completed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    workflow_sha TEXT CHECK (length(workflow_sha)<=40),
    workflow_blobs TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(workflow_blobs)),
    worker TEXT,
    heartbeat_at TEXT,
    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0,1)),
    -- Stored log totals per run. A log append checks the run limit against these
    -- columns instead of counting the run's lines on every append.
    log_lines INTEGER NOT NULL DEFAULT 0 CHECK (log_lines >= 0),
    log_bytes INTEGER NOT NULL DEFAULT 0 CHECK (log_bytes >= 0),
    -- A frozen copy of the pull request fields the run was started with.
    pull_request_event TEXT CHECK (pull_request_event IS NULL OR json_valid(pull_request_event)),
    -- The inputs a manual run was started with: a JSON object of strings, as GitHub's event.inputs.
    workflow_inputs TEXT CHECK (workflow_inputs IS NULL OR json_valid(workflow_inputs)),
    -- The attempt at a pull request revision: 1 for the first run, one more for each run started again.
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    engine TEXT CHECK (engine IN ('codeyard-actions','act')),
    workflow_run_number INTEGER,
    workflow_run_id INTEGER REFERENCES ci_runs(id) ON DELETE CASCADE,
    workflow_run_attempt INTEGER,
    log_version INTEGER NOT NULL DEFAULT 0,
    log_expires_at INTEGER CHECK (log_expires_at IS NULL OR completed_at IS NOT NULL),
    runner_labels TEXT NOT NULL DEFAULT '[]',
    runner_group TEXT,
    approval_required BOOLEAN NOT NULL DEFAULT FALSE,
    event_payload TEXT
);

CREATE UNIQUE INDEX idx_ci_runs_pull_revision ON ci_runs(pull_request_id, branch, head_sha, workflow_path, attempt)
    WHERE event = 'pull_request' AND pull_request_id IS NOT NULL;

CREATE TABLE ci_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE CHECK (run_id IS NULL OR typeof(run_id)='integer'),
    stage TEXT NOT NULL CHECK (length(stage)<=255),
    line TEXT NOT NULL,
    level TEXT NOT NULL DEFAULT 'info' CHECK (length(level)<=20), 
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    job_id INTEGER REFERENCES actions_jobs(id) ON DELETE SET NULL,
    step_number INTEGER
);

CREATE TABLE ci_deployments (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE CHECK (run_id IS NULL OR typeof(run_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    environment TEXT NOT NULL CHECK (length(environment)<=255),
    environment_key TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (length(status)<=20), 
    approved_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (approved_by_id IS NULL OR typeof(approved_by_id)='integer'),
    approved_at TEXT,
    started_at TEXT,
    completed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE auth_login_limits (
    account_key TEXT NOT NULL PRIMARY KEY CHECK (length(account_key)<=80),
    attempts INTEGER NOT NULL CHECK (attempts BETWEEN 0 AND 120) CHECK (attempts IS NULL OR typeof(attempts)='integer') CHECK (attempts BETWEEN -2147483648 AND 2147483647),
    window_ends_at TEXT NOT NULL
);

CREATE TABLE ci_accepted_pushes (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    actor_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (actor_id IS NULL OR typeof(actor_id)='integer'),
    reference TEXT NOT NULL,
    sha TEXT NOT NULL CHECK (length(sha)<=40),
    accepted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(repo_id, actor_id, reference, sha)
);
CREATE INDEX ci_accepted_pushes_time ON ci_accepted_pushes(repo_id, accepted_at);

CREATE TABLE repository_discussion_counters (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    next_number INTEGER NOT NULL CHECK (next_number BETWEEN 1 AND 2147483648) CHECK (next_number IS NULL OR typeof(next_number)='integer')
);

-- Cluster membership: machines that may talk to each other, and the invites that
-- admit new ones. Only PostgreSQL installations run more than one node; SQLite
-- carries the same tables so both engines share one set of operations.
CREATE TABLE nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    public_key TEXT NOT NULL UNIQUE CHECK (length(public_key)=64 AND public_key NOT GLOB '*[^0-9a-f]*'),
    name TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 63 AND name GLOB '[a-z0-9]*' AND name NOT GLOB '*[^a-z0-9._-]*'),
    roles TEXT NOT NULL CHECK (json_valid(roles) AND json_type(roles)='array' AND json_array_length(roles)>0),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'active', 'revoked')),
    addr_hint TEXT,
    version TEXT,
    disk_free INTEGER CHECK (disk_free IS NULL OR (typeof(disk_free)='integer' AND disk_free>=0)),
    last_seen TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    runner_managed BOOLEAN NOT NULL DEFAULT false
);

CREATE TABLE join_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash)=64 AND token_hash NOT GLOB '*[^0-9a-f]*'),
    roles TEXT NOT NULL CHECK (json_valid(roles) AND json_type(roles)='array' AND json_array_length(roles)>0),
    reusable INTEGER NOT NULL DEFAULT 0 CHECK (reusable IN (0,1)),
    requires_approval INTEGER NOT NULL DEFAULT 0 CHECK (requires_approval IN (0,1)),
    expires_at TEXT NOT NULL,
    uses INTEGER NOT NULL DEFAULT 0 CHECK (typeof(uses)='integer' AND uses>=0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    runner_scoped BOOLEAN NOT NULL DEFAULT false
);

-- A copy of a repository held by another node. It goes with its repository. applied_version -1
-- means nothing has been copied yet.
CREATE TABLE repository_replicas (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id),
    applied_version INTEGER NOT NULL DEFAULT -1 CHECK (applied_version >= -1),
    synced_at TEXT,
    PRIMARY KEY (repository_id, node_id)
);

CREATE INDEX idx_repository_replicas_node ON repository_replicas(node_id);
CREATE INDEX idx_repository_replicas_pending ON repository_replicas(node_id, repository_id)
WHERE applied_version < 0 AND synced_at IS NULL;

CREATE INDEX idx_files_user_id ON files(user_id);

CREATE INDEX idx_files_is_active ON files(is_active);

CREATE INDEX idx_files_created_at ON files(created_at);

CREATE INDEX idx_files_reason ON files(reason);

CREATE UNIQUE INDEX idx_signup_requests_username_pending
ON signup_requests (username_key)
WHERE status = 'pending';

CREATE UNIQUE INDEX idx_signup_requests_email_pending
ON signup_requests (email_key)
WHERE email IS NOT NULL AND status = 'pending';

CREATE INDEX idx_signup_requests_status_created_at
ON signup_requests (status, created_at DESC, id DESC);

CREATE INDEX idx_oauth_accounts_user_id ON oauth_accounts(user_id);

CREATE INDEX idx_sessions_user_id ON sessions(user_id);
CREATE INDEX idx_sessions_user_ordinal ON sessions(user_id,ordinal DESC);

CREATE INDEX idx_sessions_expires_at ON sessions(expires_at);

CREATE INDEX idx_anonymous_sessions_expires_at ON anonymous_sessions(expires_at);

CREATE INDEX idx_oauth_states_expires_at ON oauth_states(expires_at);

CREATE INDEX idx_ssh_keys_user_id ON ssh_keys(user_id);

CREATE INDEX idx_gpg_keys_key_id ON gpg_keys(key_id);

CREATE INDEX idx_org_memberships_user_id ON org_memberships(user_id);

CREATE INDEX idx_team_memberships_user_id ON team_memberships(user_id);

CREATE UNIQUE INDEX organizations_name_folded ON organizations(lower(name));
CREATE UNIQUE INDEX repo_unique_user_folded ON repositories(owner_user_id,lower(name));
CREATE UNIQUE INDEX repo_unique_org_folded ON repositories(owner_org_id,lower(name));
CREATE INDEX idx_repositories_name ON repositories(name);
CREATE INDEX idx_repositories_storage_uuid ON repositories (substr(disk_path, -40));

CREATE INDEX idx_repositories_is_private ON repositories(is_private);

CREATE INDEX idx_repositories_forked_from_id ON repositories(forked_from_id);

CREATE INDEX idx_repositories_created_at ON repositories(created_at);

CREATE INDEX idx_repositories_updated_at ON repositories(updated_at);
CREATE INDEX idx_repositories_created_by ON repositories(created_by_id,created_at DESC,id DESC);
CREATE INDEX idx_repositories_public_stars
    ON repositories(stars_count DESC, created_at DESC, id DESC) WHERE is_private = false;
CREATE INDEX idx_repositories_search_order
    ON repositories(stars_count DESC, updated_at DESC);

CREATE INDEX idx_repo_access_repo_id ON repo_access(repo_id);

CREATE INDEX idx_team_repo_access_repo_id ON team_repo_access(repo_id);

CREATE INDEX idx_deploy_keys_fingerprint ON deploy_keys(fingerprint);

CREATE INDEX idx_repo_storage_history_repo_time ON repo_storage_history(repo_id, recorded_at DESC);

CREATE INDEX idx_repo_storage_history_recorded_at ON repo_storage_history(recorded_at);

CREATE INDEX idx_issues_author_id ON issues(author_id,created_at DESC,id DESC);

CREATE INDEX idx_issues_state ON issues(state);

CREATE INDEX idx_issues_repo_state ON issues(repo_id, state);

CREATE INDEX idx_issues_created_at ON issues(created_at);
CREATE INDEX idx_issues_updated_at ON issues(updated_at DESC);

CREATE INDEX idx_issues_milestone_id ON issues(milestone_id);

CREATE INDEX idx_issue_comments_issue_id ON issue_comments(issue_id);

CREATE INDEX idx_issue_comments_author_id ON issue_comments(author_id, created_at);

CREATE INDEX idx_issue_comments_created_at ON issue_comments(created_at);

CREATE INDEX idx_pull_requests_author_id ON pull_requests(author_id,created_at DESC,id DESC);

CREATE INDEX idx_pull_requests_state ON pull_requests(state);

CREATE INDEX idx_pull_requests_repo_state ON pull_requests(repo_id, state);
CREATE INDEX idx_pull_requests_head_repo_state ON pull_requests(head_repo_id, state);
CREATE INDEX pull_requests_check_heads ON pull_requests(repo_id, head_sha, number) WHERE state='open' AND head_repo_id=repo_id;

CREATE INDEX idx_pull_requests_created_at ON pull_requests(created_at);
CREATE INDEX idx_pull_requests_updated_at ON pull_requests(updated_at DESC);
CREATE INDEX idx_pull_requests_repo_stack ON pull_requests(repo_id, stack_id);
CREATE INDEX idx_pull_requests_milestone_id ON pull_requests(milestone_id);

CREATE INDEX idx_pr_comments_pr_id ON pr_comments(pull_request_id);

CREATE INDEX idx_pr_comments_author_id ON pr_comments(author_id, created_at);

CREATE INDEX idx_pr_comments_created_at ON pr_comments(created_at);

CREATE INDEX idx_pr_reviews_pr_id ON pr_reviews(pull_request_id);

CREATE INDEX idx_pr_reviews_author_id ON pr_reviews(author_id);

CREATE INDEX idx_pr_reviews_created_at ON pr_reviews(created_at);

-- Each author has at most one pending review on a pull request.
CREATE UNIQUE INDEX idx_pr_reviews_one_pending ON pr_reviews(pull_request_id, author_id) WHERE state = 'pending';

CREATE INDEX idx_pr_comments_reply_to ON pr_comments(in_reply_to_comment_id);

CREATE INDEX idx_pr_comments_review_id ON pr_comments(review_id);

CREATE INDEX idx_releases_author_id ON releases(author_id);

CREATE INDEX idx_releases_created_at ON releases(created_at);

CREATE INDEX idx_stars_repo_id ON stars(repo_id);

CREATE INDEX idx_watches_repo_id ON watches(repo_id);

CREATE INDEX idx_follows_following_id ON follows(following_id,follower_id);

CREATE INDEX idx_audit_log_org_id ON audit_log(org_id);

CREATE INDEX idx_audit_log_actor_id ON audit_log(actor_id);

CREATE INDEX idx_audit_log_action ON audit_log(action);

CREATE INDEX idx_audit_log_created_at ON audit_log(created_at);

CREATE INDEX idx_upload_rate_limit_user_id ON upload_rate_limit(user_id);

CREATE INDEX idx_upload_rate_limit_uploaded_at ON upload_rate_limit(uploaded_at);

CREATE INDEX idx_upload_rate_limit_reason ON upload_rate_limit(reason);

CREATE INDEX idx_upload_rate_limit_ip ON upload_rate_limit(ip_address);
CREATE INDEX idx_upload_rate_limit_file_id ON upload_rate_limit(file_id);

CREATE INDEX idx_download_rate_limit_user_id ON download_rate_limit(user_id);

CREATE INDEX idx_download_rate_limit_downloaded_at ON download_rate_limit(downloaded_at);

CREATE INDEX idx_download_rate_limit_reason ON download_rate_limit(reason);

CREATE INDEX idx_download_rate_limit_ip ON download_rate_limit(ip_address);
CREATE INDEX idx_download_rate_limit_file_id ON download_rate_limit(file_id);

CREATE INDEX idx_notifications_user_created_at ON notifications(user_id, created_at DESC);

CREATE INDEX idx_notifications_user_unread ON notifications(user_id, read_at, created_at DESC);

CREATE INDEX idx_notifications_user_saved ON notifications(user_id, saved_at, created_at DESC);

CREATE INDEX idx_notifications_user_done ON notifications(user_id, done_at, created_at DESC);

CREATE INDEX idx_notifications_actor_id ON notifications(actor_id);

CREATE INDEX idx_ci_runs_repo_created_at ON ci_runs(repo_id, created_at DESC);
CREATE INDEX ci_runs_log_expiry ON ci_runs(log_expires_at,id) WHERE log_expires_at IS NOT NULL AND log_lines>0;

CREATE INDEX idx_ci_runs_user_created_at ON ci_runs(triggered_by_id, created_at DESC);

CREATE INDEX idx_ci_runs_status ON ci_runs(status);

CREATE INDEX idx_ci_runs_push_user_created_at ON ci_runs(triggered_by_id, created_at DESC)
    WHERE event = 'push';

CREATE INDEX idx_ci_logs_run_id_id ON ci_logs(run_id, id);

CREATE INDEX idx_ci_deployments_repo_env ON ci_deployments(repo_id, environment, created_at DESC);
CREATE INDEX ci_deployments_environment_waiting ON ci_deployments(repo_id,environment_key,status,run_id) WHERE status IN ('pending','approved');

CREATE INDEX idx_auth_login_limits_expiry ON auth_login_limits (window_ends_at);

CREATE INDEX idx_ci_runs_queue ON ci_runs(status, id)
    WHERE status IN ('queued', 'running');
CREATE INDEX idx_files_pending ON files(upload_expires_at) WHERE NOT is_active;
CREATE INDEX idx_notifications_scope ON notifications(user_id, scope_repository_id);
CREATE INDEX notifications_user_id ON notifications(user_id, id DESC);
CREATE INDEX idx_repository_pushes_actor ON repository_pushes(actor_id,created_at DESC,id DESC);

INSERT INTO file_reason_limits (reason, max_file_size_bytes, max_uploads_per_hour, max_bytes_per_hour, max_uploads_per_day, max_bytes_per_day, allowed_content_types, description) VALUES
('avatar', 2097152, 5, 10485760, 20, 41943040, '["image/jpeg", "image/png", "image/gif", "image/webp"]', 'User profile avatars'),
('org_avatar', 2097152, 5, 10485760, 20, 41943040, '["image/jpeg", "image/png", "image/gif", "image/webp"]', 'Organization avatars'),
('release_asset', 104857600, 20, 524288000, 100, 2147483648, NULL, 'Release binary assets'),
('issue_attachment', 10485760, 20, 104857600, 100, 524288000, '["image/jpeg", "image/png", "image/gif", "image/webp", "application/pdf", "text/plain", "text/markdown"]', 'Issue attachments'),
('pr_attachment', 10485760, 20, 104857600, 100, 524288000, '["image/jpeg", "image/png", "image/gif", "image/webp", "application/pdf", "text/plain", "text/markdown"]', 'PR attachments'),
('readme_image', 5242880, 10, 52428800, 50, 262144000, '["image/jpeg", "image/png", "image/gif", "image/webp", "image/svg+xml"]', 'README and wiki images'),
('repo_asset', 52428800, 10, 262144000, 50, 1073741824, NULL, 'Repository assets'),
('codeql_database', 104857600, 10, 1073741824, 40, 4294967296, '["application/zip"]', 'CodeQL databases'),
('package', 104857600, 100, 1073741824, 500, 5368709120, NULL, 'Package blob uploads'),
('other', 10485760, 5, 52428800, 20, 104857600, NULL, 'Uncategorized files')
ON CONFLICT (reason) DO NOTHING;

-- Counters are committed in the same statement as social/fork facts.
CREATE TRIGGER stars_insert AFTER INSERT ON stars BEGIN
    UPDATE repositories SET stars_count=stars_count+1 WHERE id=NEW.repo_id;
END;
CREATE TRIGGER stars_delete AFTER DELETE ON stars BEGIN
    UPDATE repositories SET stars_count=stars_count-1 WHERE id=OLD.repo_id;
END;

-- Ignoring a repository does not count as watching it.
CREATE TRIGGER watches_insert AFTER INSERT ON watches WHEN NEW.level IN ('watching','custom') BEGIN
    UPDATE repositories SET watchers_count=watchers_count+1 WHERE id=NEW.repo_id;
END;
CREATE TRIGGER watches_delete AFTER DELETE ON watches WHEN OLD.level IN ('watching','custom') BEGIN
    UPDATE repositories SET watchers_count=watchers_count-1 WHERE id=OLD.repo_id;
END;
CREATE TRIGGER watches_update AFTER UPDATE OF level,repo_id ON watches BEGIN
    UPDATE repositories SET watchers_count=watchers_count-1 WHERE id=OLD.repo_id AND OLD.level IN ('watching','custom');
    UPDATE repositories SET watchers_count=watchers_count+1 WHERE id=NEW.repo_id AND NEW.level IN ('watching','custom');
END;
CREATE TRIGGER forks_insert AFTER INSERT ON repositories WHEN NEW.forked_from_id IS NOT NULL BEGIN
    UPDATE repositories SET forks_count=forks_count+1 WHERE id=NEW.forked_from_id;
END;
CREATE TRIGGER forks_delete AFTER DELETE ON repositories WHEN OLD.forked_from_id IS NOT NULL BEGIN
    UPDATE repositories SET forks_count=forks_count-1 WHERE id=OLD.forked_from_id;
END;

-- The visibility floor survives deletion of the original source. SQLite cannot
-- assign NEW values in BEFORE triggers: native fork INSERTs also select private
-- policy in SQL, so RETURNING never claims public for a private source.
CREATE TRIGGER repository_visibility_no_widen BEFORE UPDATE OF is_private,visibility_locked,forked_from_id ON repositories
WHEN OLD.visibility_locked AND (NOT NEW.is_private OR NOT NEW.visibility_locked) BEGIN
    SELECT RAISE(ABORT,'nonpublic fork visibility cannot be widened');
END;
CREATE TRIGGER repository_visibility_insert AFTER INSERT ON repositories
WHEN EXISTS(SELECT 1 FROM repositories WHERE id=NEW.forked_from_id AND (is_private OR visibility_locked)) BEGIN
    UPDATE repositories SET is_private=1,visibility_locked=1 WHERE id=NEW.id;
END;
CREATE TRIGGER repository_visibility_update AFTER UPDATE OF forked_from_id ON repositories
WHEN EXISTS(SELECT 1 FROM repositories WHERE id=NEW.forked_from_id AND (is_private OR visibility_locked)) BEGIN
    UPDATE repositories SET is_private=1,visibility_locked=1 WHERE id=NEW.id;
END;
CREATE TRIGGER repository_visibility_descendants AFTER UPDATE OF is_private ON repositories
WHEN NEW.is_private AND NOT OLD.is_private BEGIN
    UPDATE repositories SET is_private=1,visibility_locked=1 WHERE id IN (
        WITH RECURSIVE descendants(id) AS (
            SELECT id FROM repositories WHERE forked_from_id=NEW.id
            UNION SELECT r.id FROM repositories r JOIN descendants d ON r.forked_from_id=d.id
        ) SELECT id FROM descendants
    ) AND (NOT is_private OR NOT visibility_locked);
END;

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
    username TEXT CHECK (length(username) <= 1024),
    encrypted_password BLOB,
    password_nonce BLOB CHECK ((encrypted_password IS NULL) = (password_nonce IS NULL)),
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
    -- repository_dispatch actions are up to 100 characters.
    action TEXT CHECK (length(action) <= 100),
    status_code INTEGER,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    error TEXT CHECK (length(error) <= 512),
    request_headers TEXT NOT NULL,
    request_body TEXT NOT NULL,
    response_headers TEXT,
    response_body TEXT,
    delivered_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    redelivery INTEGER NOT NULL DEFAULT 0 CHECK (redelivery IN (0,1))
);

CREATE INDEX idx_webhook_deliveries_webhook_id ON webhook_deliveries(webhook_id, id);
CREATE INDEX webhook_deliveries_expiry ON webhook_deliveries(delivered_at, id);

-- Sign-in and recovery look accounts up by address, ignoring case, so no two accounts may
-- share one.
CREATE UNIQUE INDEX idx_users_email_lower ON users(lower(email));

CREATE TABLE email_settings (
    id INTEGER PRIMARY KEY CHECK (id IS NULL OR typeof(id)='integer') CHECK (id = 1),
    host TEXT NOT NULL CHECK (length(host) BETWEEN 1 AND 255),
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    security TEXT NOT NULL CHECK (security IN ('starttls', 'tls', 'none')),
    username TEXT NOT NULL DEFAULT '' CHECK (length(username) <= 255),
    encrypted_password BLOB,
    password_nonce BLOB,
    from_name TEXT NOT NULL DEFAULT '' CHECK (length(from_name) <= 255),
    from_address TEXT NOT NULL CHECK (length(from_address) BETWEEN 1 AND 255),
    reply_to TEXT NOT NULL DEFAULT '' CHECK (length(reply_to) <= 255),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK ((encrypted_password IS NULL) = (password_nonce IS NULL))
);

CREATE TABLE email_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    to_address TEXT NOT NULL CHECK (length(to_address) <= 255),
    subject TEXT NOT NULL CHECK (length(subject) <= 998),
    body_text TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    last_error TEXT CHECK (length(last_error) <= 1000),
    sent_at TEXT,
    organization_invitation_id INTEGER REFERENCES organization_invitations(id) ON DELETE SET NULL,
    -- A notification email is sent only while its recipient can still read the notification.
    notification_id INTEGER REFERENCES notifications(id) ON DELETE CASCADE,
    link_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE CHECK (link_user_id IS NULL OR typeof(link_user_id)='integer'),
    link_purpose TEXT CHECK (link_purpose IN ('verify','reset','secondary')),
    link_token_hash TEXT CHECK (length(link_token_hash)=64),
    CHECK ((link_user_id IS NULL)=(link_purpose IS NULL) AND (link_user_id IS NULL)=(link_token_hash IS NULL))
);
CREATE INDEX email_outbox_link ON email_outbox(link_user_id,link_purpose,sent_at) WHERE link_user_id IS NOT NULL;

CREATE TABLE email_tokens (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    purpose TEXT NOT NULL CHECK (purpose IN ('verify', 'reset')),
    email TEXT NOT NULL CHECK (length(email) <= 255),
    token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
    expires_at TEXT NOT NULL,
    PRIMARY KEY (user_id, purpose)
);

CREATE TABLE repo_language_cache (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    storage_node_id INTEGER,
    write_version INTEGER NOT NULL,
    revision TEXT NOT NULL
);

CREATE TABLE actions_settings (
    id INTEGER PRIMARY KEY CHECK (id IS NULL OR typeof(id)='integer') CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    run_timeout_minutes INTEGER CHECK (run_timeout_minutes BETWEEN 1 AND 10080),
    job_cpus INTEGER CHECK (job_cpus BETWEEN 1 AND 1024),
    job_memory_gib INTEGER CHECK (job_memory_gib BETWEEN 1 AND 4096),
    runner_image TEXT CHECK (length(runner_image) BETWEEN 1 AND 255),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    results_url TEXT,
    cache_size_gib INTEGER CHECK (cache_size_gib BETWEEN 1 AND 10240),
    cache_retention_days INTEGER CHECK (cache_retention_days BETWEEN 1 AND 365),
    artifact_retention_days INTEGER CHECK (artifact_retention_days BETWEEN 1 AND 365),
    default_workflow_permissions TEXT NOT NULL DEFAULT 'restricted' CHECK (default_workflow_permissions IN ('restricted','permissive')),
    codeyard_actions_enabled INTEGER NOT NULL DEFAULT 1 CHECK (codeyard_actions_enabled IN (0,1)),
    act_enabled INTEGER NOT NULL DEFAULT 1 CHECK (act_enabled IN (0,1)),
    default_engine TEXT NOT NULL DEFAULT 'codeyard-actions' CHECK ((default_engine='codeyard-actions' AND codeyard_actions_enabled) OR (default_engine='act' AND act_enabled))
);

CREATE TABLE repo_actions_settings (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    run_timeout_minutes INTEGER CHECK (run_timeout_minutes BETWEEN 1 AND 10080),
    job_cpus INTEGER CHECK (job_cpus BETWEEN 1 AND 1024),
    job_memory_gib INTEGER CHECK (job_memory_gib BETWEEN 1 AND 4096),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    cache_size_gib INTEGER CHECK (cache_size_gib BETWEEN 1 AND 10240),
    cache_retention_days INTEGER CHECK (cache_retention_days BETWEEN 1 AND 365),
    rest_settings TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(rest_settings) AND json_type(rest_settings)='object')
);

CREATE INDEX idx_email_outbox_due ON email_outbox(next_attempt_at) WHERE sent_at IS NULL;

CREATE INDEX idx_release_assets_file_id ON release_assets(file_id);

CREATE INDEX idx_users_avatar_file ON users(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

CREATE INDEX idx_signup_requests_processed_hash ON signup_requests(id) WHERE status <> 'pending' AND password_hash <> '';

CREATE INDEX idx_organizations_avatar_file ON organizations(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

CREATE INDEX idx_repositories_storage_node ON repositories(storage_node_id, id);

CREATE INDEX idx_commit_statuses_ci_run_id ON commit_statuses(ci_run_id) WHERE ci_run_id IS NOT NULL;

CREATE INDEX idx_issue_assignees_user ON issue_assignees(user_id);

CREATE INDEX idx_pull_requests_merged_by ON pull_requests(merged_by_id,merged_at DESC,id DESC) WHERE merged_at IS NOT NULL;

CREATE INDEX idx_issue_labels_label ON issue_labels(label_id);

CREATE INDEX idx_pr_labels_label ON pr_labels(label_id);

CREATE INDEX idx_pr_assignees_user ON pr_assignees(user_id);

CREATE INDEX idx_pr_reviewers_user ON pr_reviewers(user_id);

CREATE INDEX idx_upload_rate_limit_user_reason_time ON upload_rate_limit(user_id, reason, uploaded_at);

CREATE INDEX idx_notifications_repo_id ON notifications(repo_id);

CREATE INDEX idx_ci_runs_repo_head ON ci_runs(repo_id, head_sha, id);

CREATE INDEX idx_ci_runs_pull_request ON ci_runs(pull_request_id) WHERE pull_request_id IS NOT NULL;

CREATE INDEX idx_ci_runs_push_created_at ON ci_runs(created_at) WHERE event = 'push' AND triggered_by_id IS NOT NULL;

CREATE UNIQUE INDEX idx_ci_deployments_run ON ci_deployments(run_id);

CREATE INDEX idx_repository_pushes_repo ON repository_pushes(repo_id, created_at);

CREATE INDEX idx_repository_pushes_created_at ON repository_pushes(created_at);

CREATE TABLE repository_star_days (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day TEXT NOT NULL,
    stars INTEGER NOT NULL CHECK (stars >= 0),
    PRIMARY KEY (repo_id, day)
);
CREATE TRIGGER record_repository_star_day AFTER INSERT ON stars BEGIN
    INSERT INTO repository_star_days (repo_id,day,stars) VALUES (NEW.repo_id,date(NEW.created_at),1)
    ON CONFLICT(repo_id,day) DO UPDATE SET stars=stars+1;
END;

CREATE TABLE pull_request_stacks (
    id INTEGER PRIMARY KEY REFERENCES pull_requests(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    base_branch TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(repo_id,number)
);

CREATE TABLE user_social_accounts (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK (length(provider) BETWEEN 1 AND 50),
    url TEXT NOT NULL CHECK (length(url) BETWEEN 1 AND 255),
    PRIMARY KEY (user_id, url)
);

CREATE TABLE user_profile_settings (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    company TEXT CHECK (length(company) <= 255),
    twitter_username TEXT CHECK (length(twitter_username) <= 255),
    hireable INTEGER CHECK (hireable IN (0,1)),
    public_email TEXT CHECK (length(public_email) <= 255),
    primary_visibility TEXT NOT NULL DEFAULT 'private' CHECK (primary_visibility IN ('private', 'public'))
);

CREATE TABLE ssh_signing_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 100),
    key TEXT NOT NULL CHECK (length(key) <= 32768),
    fingerprint TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE INDEX idx_ssh_signing_keys_user ON ssh_signing_keys(user_id, id);

CREATE TABLE user_emails (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email TEXT NOT NULL CHECK (length(email) BETWEEN 1 AND 255),
    verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0,1)),
    verification_hash TEXT UNIQUE CHECK (length(verification_hash)=64),
    verification_expires_at TEXT,
    last_sent_at TEXT,
    PRIMARY KEY (user_id, email)
);
CREATE UNIQUE INDEX idx_user_emails_address ON user_emails(lower(email));

CREATE TRIGGER users_primary_email_insert BEFORE INSERT ON users
WHEN EXISTS (SELECT 1 FROM user_emails WHERE lower(email)=lower(NEW.email) AND user_id<>NEW.id)
BEGIN
    SELECT RAISE(ABORT,'Email address is already registered');
END;
CREATE TRIGGER users_primary_email_update BEFORE UPDATE OF email ON users
WHEN EXISTS (SELECT 1 FROM user_emails WHERE lower(email)=lower(NEW.email) AND user_id<>NEW.id)
BEGIN
    SELECT RAISE(ABORT,'Email address is already registered');
END;

CREATE TABLE user_blocks (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    blocked_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY (user_id,blocked_id),
    CHECK (user_id<>blocked_id)
);
CREATE INDEX idx_user_blocks_target ON user_blocks(blocked_id,user_id);
CREATE INDEX idx_gpg_keys_user ON gpg_keys(user_id,primary_key_id,id);

CREATE TABLE interaction_limits (
    user_id INTEGER UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    org_id INTEGER UNIQUE REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id INTEGER UNIQUE REFERENCES repositories(id) ON DELETE CASCADE,
    restriction TEXT NOT NULL CHECK (restriction IN ('existing_users','contributors_only','collaborators_only')),
    expires_at TEXT NOT NULL,
    CHECK ((user_id IS NOT NULL)+(org_id IS NOT NULL)+(repo_id IS NOT NULL)=1)
);

CREATE TABLE interaction_contributors (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    write_version INTEGER NOT NULL,
    default_branch TEXT NOT NULL,
    storage_node_id INTEGER,
    emails_hash TEXT NOT NULL,
    contributed INTEGER NOT NULL CHECK (contributed IN (0,1)),
    PRIMARY KEY(repo_id,user_id)
);

CREATE TABLE interaction_pull_caps (
    org_id INTEGER UNIQUE REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id INTEGER UNIQUE REFERENCES repositories(id) ON DELETE CASCADE,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0,1)),
    max_open_pull_requests INTEGER NOT NULL DEFAULT 1 CHECK (max_open_pull_requests BETWEEN 1 AND 1000),
    include_drafts INTEGER NOT NULL DEFAULT 0 CHECK (include_drafts IN (0,1)),
    CHECK ((org_id IS NOT NULL)+(repo_id IS NOT NULL)=1)
);

CREATE TABLE interaction_pull_bypass (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY(repo_id,user_id)
);
CREATE INDEX idx_pull_cap_author ON pull_requests(author_id,state,repo_id,is_draft);

-- Retain issued names so deleted gist URLs cannot resolve to legacy repositories.
CREATE TABLE gist_identifiers (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id)=32)
);

CREATE TABLE gists (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    id TEXT NOT NULL UNIQUE REFERENCES gist_identifiers(id) CHECK (length(id)=32),
    is_public INTEGER NOT NULL DEFAULT 0 CHECK (is_public IN (0,1)),
    description TEXT NOT NULL DEFAULT '' CHECK (length(description)<=65535),
    ready INTEGER NOT NULL DEFAULT 0 CHECK (ready IN (0,1)),
    pending_sha TEXT,
    pending_base TEXT,
    pending_description TEXT,
    pending_storage_node_id INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK ((pending_sha IS NULL)=(pending_description IS NULL))
);
CREATE INDEX idx_gists_updated ON gists(updated_at DESC,id DESC);

CREATE TABLE gist_stars (
    repo_id INTEGER NOT NULL REFERENCES gists(repo_id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(repo_id,user_id)
);
CREATE INDEX idx_gist_stars_user ON gist_stars(user_id,repo_id);

CREATE TABLE gist_comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES gists(repo_id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65535),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX idx_gist_comments_repo ON gist_comments(repo_id,id);

CREATE TABLE issue_comment_pins (
    issue_id INTEGER NOT NULL PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE CHECK (typeof(issue_id)='integer'),
    comment_id INTEGER NOT NULL UNIQUE REFERENCES issue_comments(id) ON DELETE CASCADE CHECK (typeof(comment_id)='integer'),
    pinned_by INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (pinned_by IS NULL OR typeof(pinned_by)='integer'),
    pinned_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE issue_attribute_guard (id INTEGER PRIMARY KEY CHECK(id=1));
INSERT INTO issue_attribute_guard VALUES(1);

CREATE TABLE issue_attributes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('type','field')),
    name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 128),
    description TEXT,
    data_type TEXT CHECK(data_type IN ('text','number','date','single_select','multi_select')),
    visibility TEXT NOT NULL DEFAULT 'all' CHECK(visibility IN ('all','organization_members_only')),
    is_enabled INTEGER NOT NULL DEFAULT 1 CHECK(is_enabled IN (0,1)),
    color TEXT CHECK(color IN ('gray','blue','green','yellow','orange','red','pink','purple')),
    created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CHECK((owner_user_id IS NOT NULL)+(owner_org_id IS NOT NULL)=1),
    CHECK((kind='type' AND data_type IS NULL) OR (kind='field' AND data_type IS NOT NULL AND owner_org_id IS NOT NULL))
);
CREATE UNIQUE INDEX issue_attribute_user_name ON issue_attributes(owner_user_id,kind,lower(name)) WHERE owner_user_id IS NOT NULL;
CREATE UNIQUE INDEX issue_attribute_org_name ON issue_attributes(owner_org_id,kind,lower(name)) WHERE owner_org_id IS NOT NULL;

CREATE TABLE issue_attribute_options (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attribute_id INTEGER NOT NULL REFERENCES issue_attributes(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    color TEXT NOT NULL CHECK(color IN ('gray','blue','green','yellow','orange','red','pink','purple')),
    priority INTEGER NOT NULL CHECK(typeof(priority)='integer'),
    description TEXT CHECK (length(description)<=1024)
);
CREATE UNIQUE INDEX issue_attribute_option_name ON issue_attribute_options(attribute_id,lower(name));

CREATE TABLE issue_type_assignments (
    issue_id INTEGER PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE,
    attribute_id INTEGER NOT NULL REFERENCES issue_attributes(id) ON DELETE CASCADE
);

CREATE TABLE issue_field_values (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    attribute_id INTEGER NOT NULL REFERENCES issue_attributes(id) ON DELETE CASCADE,
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    UNIQUE(issue_id,attribute_id)
);

CREATE TABLE issue_relation_guard (id INTEGER PRIMARY KEY CHECK (id=1));
INSERT INTO issue_relation_guard VALUES (1);

CREATE TABLE issue_relations (
    kind TEXT NOT NULL CHECK (kind IN ('parent','blocks','related')),
    source_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE CHECK (typeof(source_id)='integer'),
    target_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE CHECK (typeof(target_id)='integer'),
    position INTEGER NOT NULL DEFAULT 0 CHECK (typeof(position)='integer' AND position>=0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(kind,source_id,target_id),
    CHECK (source_id<>target_id),
    CHECK (kind<>'related' OR source_id<target_id)
);
CREATE UNIQUE INDEX issue_one_parent ON issue_relations(target_id) WHERE kind='parent';
CREATE INDEX issue_relations_target ON issue_relations(kind,target_id);
CREATE INDEX issue_relations_added_source ON issue_relations(kind,source_id,created_at,target_id);
CREATE INDEX issue_relations_added_target ON issue_relations(kind,target_id,created_at,source_id);

CREATE TABLE pr_comment_pins (
    pull_request_id INTEGER NOT NULL PRIMARY KEY REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (typeof(pull_request_id)='integer'),
    comment_id INTEGER NOT NULL UNIQUE REFERENCES pr_comments(id) ON DELETE CASCADE CHECK (typeof(comment_id)='integer'),
    pinned_by INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (pinned_by IS NULL OR typeof(pinned_by)='integer'),
    pinned_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

-- issues: reviewable agent proposals
CREATE TABLE repository_issue_suggestions (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1))
);

CREATE TABLE issue_suggestions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    action TEXT NOT NULL CHECK(action IN ('set_type','add_label','add_field','add_assignee','close_issue')),
    slot TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','applied','approved','dismissed','replaced','invalidated')),
    target_id INTEGER CHECK(target_id IS NULL OR (typeof(target_id)='integer' AND target_id>0)),
    target_name TEXT NOT NULL,
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    rationale TEXT,
    confidence TEXT CHECK(confidence IN ('LOW','MEDIUM','HIGH')),
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    issue_event_id INTEGER REFERENCES timeline_events(id) ON DELETE SET NULL,
    resolved_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX issue_suggestions_state ON issue_suggestions(issue_id,state,id);
CREATE INDEX issue_suggestions_history ON issue_suggestions(issue_id,created_at,id);
CREATE INDEX issue_suggestions_actor ON issue_suggestions(issue_id,actor_id,created_at,id);
CREATE INDEX issue_suggestions_target ON issue_suggestions(action,target_id) WHERE state='pending';
CREATE UNIQUE INDEX issue_suggestions_current_actor_slot ON issue_suggestions(issue_id,actor_id,slot) WHERE state='pending';
CREATE TRIGGER issue_suggestion_attribute_deleted AFTER DELETE ON issue_attributes BEGIN
    UPDATE issue_suggestions SET state='invalidated',updated_at=MAX(strftime('%Y-%m-%dT%H:%M:%f000Z','now'),strftime('%Y-%m-%dT%H:%M:%f000Z',updated_at,'+0.001 seconds')) WHERE target_id=OLD.id AND state='pending' AND action=CASE WHEN OLD.kind='type' THEN 'set_type' ELSE 'add_field' END;
END;
CREATE TRIGGER issue_suggestion_label_deleted AFTER DELETE ON labels BEGIN
    UPDATE issue_suggestions SET state='invalidated',updated_at=MAX(strftime('%Y-%m-%dT%H:%M:%f000Z','now'),strftime('%Y-%m-%dT%H:%M:%f000Z',updated_at,'+0.001 seconds')) WHERE target_id=OLD.id AND state='pending' AND action='add_label';
END;
CREATE TRIGGER issue_suggestion_assignee_deleted AFTER DELETE ON users BEGIN
    UPDATE issue_suggestions SET state='invalidated',updated_at=MAX(strftime('%Y-%m-%dT%H:%M:%f000Z','now'),strftime('%Y-%m-%dT%H:%M:%f000Z',updated_at,'+0.001 seconds')) WHERE target_id=OLD.id AND state='pending' AND action='add_assignee';
END;
CREATE TRIGGER issue_suggestion_duplicate_deleted AFTER DELETE ON issues BEGIN
    UPDATE issue_suggestions SET state='invalidated',updated_at=MAX(strftime('%Y-%m-%dT%H:%M:%f000Z','now'),strftime('%Y-%m-%dT%H:%M:%f000Z',updated_at,'+0.001 seconds')) WHERE target_id=OLD.id AND state='pending' AND action='close_issue';
END;
CREATE TRIGGER issue_suggestion_definition_changed AFTER UPDATE ON issue_attributes BEGIN
    UPDATE issue_suggestions SET state='invalidated',updated_at=MAX(strftime('%Y-%m-%dT%H:%M:%f000Z','now'),strftime('%Y-%m-%dT%H:%M:%f000Z',updated_at,'+0.001 seconds')) WHERE target_id=NEW.id AND state='pending' AND ((NEW.kind='type' AND OLD.is_enabled AND NOT NEW.is_enabled AND action='set_type') OR (NEW.kind='field' AND action='add_field'));
END;

-- pulls
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    is_public BOOLEAN NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX events_created ON events(created_at DESC,id DESC);
CREATE INDEX events_repository ON events(repo_id,created_at DESC,id DESC);
CREATE INDEX events_actor ON events(actor_id,created_at DESC,id DESC);

-- pulls
CREATE TABLE activity_feed_tokens (
    api_token_id INTEGER PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    value TEXT NOT NULL UNIQUE
);

-- pulls
CREATE TABLE pull_request_async_merges (
    uuid TEXT PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    token_id INTEGER REFERENCES personal_access_tokens(id) ON DELETE SET NULL,
    worker_node_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    number INTEGER NOT NULL,
    request TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','merged','failed')),
    details TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    expires_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now','+1 day'))
);
CREATE UNIQUE INDEX pull_request_async_pending ON pull_request_async_merges(pull_request_id) WHERE status='pending';
CREATE INDEX pull_request_async_expiry ON pull_request_async_merges(expires_at);

-- pulls
CREATE TABLE notification_threads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    scope_repository_id INTEGER NOT NULL CHECK(scope_repository_id>0),
    subject_type TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(user_id,scope_repository_id,subject_type,subject_key)
);
CREATE INDEX notification_threads_subject ON notification_threads(scope_repository_id,subject_type,subject_key,user_id);

CREATE TABLE notification_thread_subscriptions (
    thread_id INTEGER PRIMARY KEY REFERENCES notification_threads(id) ON DELETE CASCADE,
    is_explicit BOOLEAN NOT NULL DEFAULT FALSE,
    subscribed BOOLEAN NOT NULL DEFAULT TRUE,
    ignored BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX notifications_thread ON notifications(thread_id,id DESC);
CREATE INDEX notifications_thread_created ON notifications(thread_id,created_at DESC,id DESC);
CREATE INDEX notifications_thread_inbox ON notifications(thread_id,id DESC) WHERE done_at IS NULL;
CREATE INDEX notifications_thread_unread ON notifications(thread_id,id DESC) WHERE read_at IS NULL AND done_at IS NULL;
CREATE TRIGGER notification_threads_provenance BEFORE UPDATE OF user_id,scope_repository_id,subject_type,subject_key ON notification_threads
WHEN NEW.user_id<>OLD.user_id OR NEW.scope_repository_id<>OLD.scope_repository_id OR NEW.subject_type<>OLD.subject_type OR NEW.subject_key<>OLD.subject_key
BEGIN SELECT RAISE(ABORT,'notification thread provenance is immutable'); END;
CREATE TRIGGER notifications_thread_insert BEFORE INSERT ON notifications
WHEN NEW.thread_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM notification_threads t WHERE t.id=NEW.thread_id AND t.user_id=NEW.user_id AND t.scope_repository_id=NEW.scope_repository_id)
BEGIN SELECT RAISE(ABORT,'notification thread scope mismatch'); END;
CREATE TRIGGER notifications_thread_update BEFORE UPDATE OF thread_id ON notifications
WHEN NEW.thread_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM notification_threads t WHERE t.id=NEW.thread_id AND t.user_id=NEW.user_id AND t.scope_repository_id=NEW.scope_repository_id)
BEGIN SELECT RAISE(ABORT,'notification thread scope mismatch'); END;
CREATE UNIQUE INDEX pull_requests_source_issue ON pull_requests(source_issue_id) WHERE source_issue_id IS NOT NULL;
CREATE UNIQUE INDEX pr_comments_source_issue ON pr_comments(source_issue_comment_id) WHERE source_issue_comment_id IS NOT NULL;

CREATE TABLE actions_runtime_tokens (
    run_id INTEGER PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    backend_id TEXT NOT NULL UNIQUE
);

CREATE TABLE actions_objects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    run_id INTEGER REFERENCES ci_runs(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (kind IN ('cache','artifact')),
    scope TEXT NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 512),
    version TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0 CHECK (size >= 0),
    finalized INTEGER NOT NULL DEFAULT 0 CHECK (finalized IN (0,1)),
    manifest TEXT,
    created_at INTEGER NOT NULL,
    accessed_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX actions_cache_key ON actions_objects(repo_id,scope,name,version) WHERE kind='cache';
CREATE UNIQUE INDEX actions_artifact_name ON actions_objects(run_id,name) WHERE kind='artifact';
CREATE INDEX actions_objects_expiry ON actions_objects(expires_at);
CREATE INDEX actions_objects_run ON actions_objects(run_id);
CREATE TRIGGER actions_run_delete BEFORE DELETE ON ci_runs BEGIN
    DELETE FROM actions_objects WHERE run_id=OLD.id AND (kind='artifact' OR NOT finalized);
END;

CREATE TABLE actions_blob_parts (
    object_id INTEGER NOT NULL REFERENCES actions_objects(id) ON DELETE CASCADE,
    block_id TEXT NOT NULL,
    object_key TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL CHECK (size >= 0),
    ready INTEGER NOT NULL DEFAULT 0 CHECK (ready IN (0,1)),
    PRIMARY KEY(object_id,block_id)
);
CREATE TRIGGER actions_blob_delete AFTER DELETE ON actions_blob_parts BEGIN
    INSERT INTO pending_object_deletions(object_key,delete_after)
    VALUES(OLD.object_key,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+7 days'))
    ON CONFLICT(object_key) DO NOTHING;
END;

CREATE TABLE actions_blob_grants (
    token_hash TEXT PRIMARY KEY,
    object_id INTEGER NOT NULL REFERENCES actions_objects(id) ON DELETE CASCADE,
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    write INTEGER NOT NULL CHECK (write IN (0,1)),
    expires_at INTEGER NOT NULL
);
CREATE INDEX actions_blob_grants_expiry ON actions_blob_grants(expires_at);

CREATE TABLE actions_github_tokens (
    token_id INTEGER PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    job TEXT NOT NULL,
    permissions TEXT NOT NULL,
    revoked_at INTEGER
);
CREATE INDEX actions_github_tokens_run ON actions_github_tokens(run_id);
CREATE TRIGGER actions_bot_identity BEFORE UPDATE ON users
WHEN (OLD.username='github-actions[bot]' AND (NEW.username<>'github-actions[bot]' OR NEW.is_admin OR NEW.password_hash IS NOT NULL OR NEW.email IS NOT NULL)) OR (OLD.username<>'github-actions[bot]' AND NEW.username='github-actions[bot]') BEGIN
    SELECT RAISE(ABORT,'Actions bot identity is reserved');
END;
CREATE TRIGGER actions_bot_session BEFORE INSERT ON sessions
WHEN EXISTS(SELECT 1 FROM users WHERE id=NEW.user_id AND username='github-actions[bot]') BEGIN
    SELECT RAISE(ABORT,'Actions bot cannot start browser sessions');
END;
CREATE TRIGGER actions_github_tokens_delete BEFORE DELETE ON ci_runs BEGIN
    DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM actions_github_tokens WHERE run_id=OLD.id);
END;

-- actions-runs
CREATE TABLE org_actions_settings (
    org_id INTEGER PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    cache_size_gib INTEGER CHECK (cache_size_gib IS NULL OR (typeof(cache_size_gib)='integer' AND cache_size_gib>0)),
    cache_retention_days INTEGER CHECK (cache_retention_days IS NULL OR (typeof(cache_retention_days)='integer' AND cache_retention_days>0)),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    rest_settings TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(rest_settings) AND json_type(rest_settings)='object')
);

CREATE TABLE actions_workflows (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','disabled_manually')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    next_run_number INTEGER NOT NULL DEFAULT 0,
    UNIQUE(repo_id,path)
);

-- graphql
CREATE TABLE graphql_rate_limits (
    token_id INTEGER PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    used INTEGER NOT NULL CHECK (typeof(used)='integer' AND used BETWEEN 1 AND 5000),
    reset_at TEXT NOT NULL
);
CREATE INDEX ci_runs_workflow_run ON ci_runs(workflow_run_id,workflow_run_attempt);
CREATE INDEX ci_runs_logical_latest ON ci_runs(repo_id,(COALESCE(workflow_run_id,id)) DESC,id DESC);

CREATE TABLE actions_run_metadata (
    run_id INTEGER PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    workflow_id INTEGER NOT NULL REFERENCES actions_workflows(id),
    head_repository TEXT CHECK(head_repository IS NULL OR json_valid(head_repository)),
    head_owner TEXT
);

-- External checks and Actions jobs allocate their public check-run identity here.
CREATE TABLE check_run_identities (id INTEGER PRIMARY KEY AUTOINCREMENT);

-- actions-runs: observed executions, not synthetic workflow plans
CREATE TABLE actions_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    check_run_id BIGINT NOT NULL UNIQUE REFERENCES check_run_identities(id),
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    generation TEXT NOT NULL,
    workflow_path TEXT NOT NULL,
    workflow_name TEXT,
    matrix TEXT CHECK(matrix IS NULL OR json_valid(matrix)),
    logical_job TEXT NOT NULL,
    variant INTEGER NOT NULL CHECK(variant>=0),
    name TEXT NOT NULL,
    labels TEXT NOT NULL CHECK(json_valid(labels)),
    status TEXT NOT NULL CHECK(status IN ('queued','in_progress','completed')),
    conclusion TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT,
    duration_ms INTEGER CHECK(duration_ms>=0),
    UNIQUE(run_id,generation,workflow_path,logical_job,variant)
);
CREATE INDEX actions_jobs_run ON actions_jobs(run_id,id);

CREATE TABLE actions_steps (
    job_id INTEGER NOT NULL REFERENCES actions_jobs(id) ON DELETE CASCADE,
    number INTEGER NOT NULL CHECK(number>0),
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('in_progress','completed')),
    conclusion TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    duration_ms INTEGER CHECK(duration_ms>=0),
    PRIMARY KEY(job_id,number)
);

CREATE TABLE actions_step_summaries (
    job_id INTEGER NOT NULL REFERENCES actions_jobs(id) ON DELETE CASCADE,
    number INTEGER NOT NULL CHECK (number > 0 AND number <= 10000),
    markdown TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes >= 0 AND bytes <= 65536),
    PRIMARY KEY (job_id, number)
);

CREATE TABLE actions_job_states (
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    generation TEXT NOT NULL,
    workflow_path TEXT NOT NULL,
    state TEXT NOT NULL CHECK(json_valid(state)),
    PRIMARY KEY(run_id,generation,workflow_path)
);
CREATE INDEX ci_logs_job ON ci_logs(job_id,id);

CREATE TABLE actions_log_downloads (
    token_hash TEXT PRIMARY KEY,
    resource TEXT NOT NULL,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    job_id INTEGER REFERENCES actions_jobs(id) ON DELETE CASCADE,
    step_number INTEGER,
    log_version INTEGER NOT NULL,
    max_log_id INTEGER NOT NULL,
    manifest TEXT NOT NULL CHECK(json_valid(manifest)),
    expires_at INTEGER NOT NULL
);
CREATE INDEX actions_log_downloads_expiry ON actions_log_downloads(expires_at);

CREATE TABLE actions_replays (
    run_id INTEGER PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    source_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    data TEXT CHECK(data IS NULL OR json_valid(data)),
    debug INTEGER NOT NULL DEFAULT 0 CHECK(debug IN(0,1))
);

-- git
CREATE TABLE commit_status_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    status_id INTEGER NOT NULL REFERENCES commit_statuses(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    context TEXT NOT NULL,
    state TEXT NOT NULL,
    description TEXT,
    target_url TEXT,
    creator_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_commit_status_history_page ON commit_status_history(repo_id, sha, id DESC);
CREATE INDEX idx_commit_status_history_latest ON commit_status_history(status_id, id DESC);
CREATE INDEX idx_commit_status_history_context ON commit_status_history(repo_id, sha, context, id DESC);
CREATE TRIGGER commit_status_history_insert AFTER INSERT ON commit_statuses BEGIN
    INSERT INTO commit_status_history(status_id,repo_id,sha,context,state,description,target_url,creator_id,created_at)
        VALUES(NEW.id,NEW.repo_id,NEW.sha,NEW.context,NEW.state,NEW.description,NEW.target_url,NEW.creator_id,strftime('%Y-%m-%dT%H:%M:%f000Z','now'));
END;
CREATE TRIGGER commit_status_history_update AFTER UPDATE OF state,description,target_url,creator_id,updated_at ON commit_statuses BEGIN
    INSERT INTO commit_status_history(status_id,repo_id,sha,context,state,description,target_url,creator_id,created_at)
        VALUES(NEW.id,NEW.repo_id,NEW.sha,NEW.context,NEW.state,NEW.description,NEW.target_url,NEW.creator_id,strftime('%Y-%m-%dT%H:%M:%f000Z','now'));
END;
CREATE VIEW commit_statuses_latest AS
    SELECT h.id,h.repo_id,h.sha,h.context,h.state,h.description,h.target_url,h.creator_id,h.created_at,h.created_at AS updated_at
    FROM commit_status_history h JOIN commit_statuses s ON s.id=h.status_id
    WHERE h.id=(SELECT MAX(id) FROM commit_status_history WHERE status_id=s.id);

CREATE TABLE commit_comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    sha TEXT NOT NULL CHECK (length(sha)=40 AND sha NOT GLOB '*[^0-9a-f]*'),
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65536 AND length(CAST(body AS BLOB))<=262144 AND instr(body,char(0))=0),
    path TEXT CHECK (path IS NULL OR length(CAST(path AS BLOB)) BETWEEN 1 AND 4096),
    position INTEGER CHECK (position IS NULL OR (typeof(position)='integer' AND position>0)),
    line INTEGER CHECK (line IS NULL OR (typeof(line)='integer' AND line>0)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK (path IS NOT NULL OR (position IS NULL AND line IS NULL))
);
CREATE INDEX commit_comments_repository ON commit_comments(repo_id,id);
CREATE INDEX commit_comments_commit ON commit_comments(repo_id,sha,id);
CREATE TRIGGER commit_comment_reactions AFTER DELETE ON commit_comments BEGIN
    DELETE FROM reactions WHERE subject_kind='commit_comment' AND subject_id=OLD.id;
END;

CREATE INDEX pull_requests_commit_associations ON pull_requests(repo_id,id DESC) WHERE state='open' OR merged;
CREATE TRIGGER milestones_created AFTER INSERT ON milestones WHEN NEW.updated_at IS NULL BEGIN
    UPDATE milestones SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=NEW.id;
END;
CREATE TRIGGER milestones_updated AFTER UPDATE ON milestones
WHEN NEW.updated_at IS NULL OR OLD.title IS NOT NEW.title OR OLD.description IS NOT NEW.description OR OLD.due_date IS NOT NEW.due_date OR OLD.due_on IS NOT NEW.due_on OR OLD.state IS NOT NEW.state OR OLD.closed_at IS NOT NEW.closed_at OR OLD.creator_id IS NOT NEW.creator_id
BEGIN
    UPDATE milestones SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=NEW.id;
END;

-- Catalog timestamps belong to the catalog record, not an invented historical first use.
CREATE TABLE topics (
    name TEXT PRIMARY KEY NOT NULL CHECK (length(name) BETWEEN 1 AND 35 AND name NOT GLOB '*[^a-z0-9-]*'),
    display_name TEXT, short_description TEXT, description TEXT, created_by TEXT, released TEXT,
    featured BOOLEAN NOT NULL DEFAULT FALSE, curated BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE TRIGGER repository_topic_catalog AFTER INSERT ON repository_topics BEGIN
    INSERT INTO topics(name) VALUES (NEW.topic) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER label_timestamp AFTER UPDATE OF name,color,description ON labels
WHEN OLD.name<>NEW.name OR OLD.color<>NEW.color OR OLD.description IS NOT NEW.description BEGIN
    UPDATE labels SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=NEW.id;
END;

CREATE TABLE team_invitations (
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    invitation_id INTEGER NOT NULL REFERENCES organization_invitations(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('member','maintainer')),
    PRIMARY KEY (team_id, invitation_id)
);
CREATE INDEX team_invitations_invitation ON team_invitations(invitation_id);
CREATE INDEX teams_parent ON teams(parent_team_id,id);

-- Hierarchy writes maintain the closure; reads seek the relevant teams or memberships.
CREATE TABLE team_ancestors (
    team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    ancestor_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    PRIMARY KEY(team_id,ancestor_id)
);
CREATE INDEX team_ancestors_ancestor ON team_ancestors(ancestor_id,team_id);
CREATE TRIGGER team_ancestors_insert AFTER INSERT ON teams BEGIN
    INSERT INTO team_ancestors(team_id,ancestor_id)
        SELECT NEW.id,NEW.id WHERE NEW.privacy IN ('visible','secret');
    INSERT INTO team_ancestors(team_id,ancestor_id)
        SELECT NEW.id,a.ancestor_id FROM teams p JOIN team_ancestors a ON a.team_id=p.id
        WHERE p.id=NEW.parent_team_id AND p.org_id=NEW.org_id AND p.privacy='visible' AND NEW.privacy='visible';
END;
CREATE TRIGGER team_ancestors_update AFTER UPDATE OF parent_team_id,privacy,org_id ON teams
WHEN OLD.parent_team_id IS NOT NEW.parent_team_id OR OLD.privacy IS NOT NEW.privacy OR OLD.org_id IS NOT NEW.org_id BEGIN
    DELETE FROM team_ancestors WHERE team_id IN (
        WITH RECURSIVE subtree(id) AS (
            SELECT NEW.id UNION SELECT t.id FROM teams t JOIN subtree s ON t.parent_team_id=s.id
        ) SELECT id FROM subtree
    );
    INSERT INTO team_ancestors(team_id,ancestor_id)
    WITH RECURSIVE subtree(id) AS (
        SELECT NEW.id UNION SELECT t.id FROM teams t JOIN subtree s ON t.parent_team_id=s.id
    ), paths(team_id,ancestor_id) AS (
        SELECT t.id,t.id FROM teams t JOIN subtree s ON s.id=t.id WHERE t.privacy IN ('visible','secret')
        UNION
        SELECT a.team_id,p.id FROM paths a JOIN teams t ON t.id=a.ancestor_id
        JOIN teams p ON p.id=t.parent_team_id AND p.org_id=t.org_id
        WHERE t.privacy='visible' AND p.privacy='visible'
    ) SELECT team_id,ancestor_id FROM paths;
END;

CREATE VIEW effective_team_memberships AS
SELECT MIN(tm.id) AS id,a.ancestor_id AS team_id,tm.user_id,
    CASE WHEN MAX(CASE WHEN tm.team_id=a.ancestor_id AND tm.role='maintainer' THEN 1 ELSE 0 END)=1 THEN 'maintainer' ELSE 'member' END AS role,
    MIN(tm.created_at) AS created_at
FROM team_ancestors a JOIN team_memberships tm ON tm.team_id=a.team_id
JOIN teams t ON t.id=tm.team_id
JOIN org_memberships om ON om.org_id=t.org_id AND om.user_id=tm.user_id
JOIN users u ON u.id=tm.user_id
WHERE tm.role IN ('member','maintainer') AND om.role IN ('member','admin','owner')
    AND NOT u.is_banned AND NOT u.must_change_password
GROUP BY a.ancestor_id,tm.user_id;

CREATE VIEW effective_team_repo_access AS
SELECT MIN(g.id) AS id,a.team_id,g.repo_id,MAX(g.permission) AS permission,MIN(g.created_at) AS created_at
FROM team_ancestors a JOIN team_repo_access g ON g.team_id=a.ancestor_id
JOIN teams t ON t.id=g.team_id
JOIN repositories r ON r.id=g.repo_id AND r.owner_kind='org' AND r.owner_org_id=t.org_id AND r.owner_user_id IS NULL
WHERE g.permission BETWEEN 1 AND 5
GROUP BY a.team_id,g.repo_id;

-- Actions runners
CREATE TABLE action_runner_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'all' CHECK(visibility IN ('all','private','selected')),
    is_default BOOLEAN NOT NULL DEFAULT false,
    allows_public_repositories BOOLEAN NOT NULL DEFAULT false,
    restricted_to_workflows BOOLEAN NOT NULL DEFAULT false,
    selected_workflows TEXT NOT NULL DEFAULT '[]'
);
CREATE UNIQUE INDEX action_runner_groups_name ON action_runner_groups(org_id,lower(name));
CREATE UNIQUE INDEX action_runner_groups_default ON action_runner_groups(org_id) WHERE is_default;

CREATE TABLE action_runner_group_repositories (
    group_id BIGINT NOT NULL REFERENCES action_runner_groups(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(group_id,repo_id)
);
CREATE INDEX action_runner_group_repositories_repo ON action_runner_group_repositories(repo_id);

CREATE TABLE action_runners (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    group_id BIGINT REFERENCES action_runner_groups(id) ON DELETE SET NULL,
    node_id BIGINT UNIQUE REFERENCES nodes(id) ON DELETE SET NULL,
    name TEXT NOT NULL,
    os TEXT NOT NULL DEFAULT 'unknown',
    labels TEXT NOT NULL DEFAULT '[]',
    ephemeral BOOLEAN NOT NULL DEFAULT false,
    used BOOLEAN NOT NULL DEFAULT false,
    CHECK((org_id IS NOT NULL AND repo_id IS NULL) OR (org_id IS NULL AND repo_id IS NOT NULL))
);
CREATE UNIQUE INDEX action_runners_org_name ON action_runners(org_id,lower(name)) WHERE org_id IS NOT NULL;
CREATE UNIQUE INDEX action_runners_repo_name ON action_runners(repo_id,lower(name)) WHERE repo_id IS NOT NULL;
CREATE INDEX action_runners_group ON action_runners(group_id);

CREATE TABLE action_runner_tokens (
    token_hash TEXT PRIMARY KEY REFERENCES join_tokens(token_hash) ON DELETE CASCADE,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    group_id BIGINT REFERENCES action_runner_groups(id) ON DELETE SET NULL,
    runner_id BIGINT REFERENCES action_runners(id) ON DELETE CASCADE,
    CHECK((org_id IS NOT NULL AND repo_id IS NULL) OR (org_id IS NULL AND repo_id IS NOT NULL))
);

CREATE TABLE action_runner_remove_tokens (
    token_hash TEXT PRIMARY KEY,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    CHECK((org_id IS NOT NULL AND repo_id IS NULL) OR (org_id IS NULL AND repo_id IS NOT NULL))
);

CREATE TABLE repo_actions_keys (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BLOB NOT NULL CHECK (length(encrypted_key)=48),
    nonce BLOB NOT NULL CHECK (length(nonce)=12)
);

CREATE TABLE repo_actions_variables (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    value TEXT NOT NULL CHECK (length(CAST(value AS BLOB))<=49152),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(repo_id,name)
);

-- apps
CREATE TABLE oauth_applications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL UNIQUE,
    secret_hash TEXT NOT NULL,
    name TEXT NOT NULL,
    homepage TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    is_github_app BOOLEAN NOT NULL DEFAULT 0 CHECK(is_github_app IN (0,1))
);

CREATE TABLE oauth_app_grants (
    hash TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    user_code TEXT NOT NULL UNIQUE,
    scopes TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    interval INTEGER NOT NULL DEFAULT 5,
    last_poll INTEGER NOT NULL DEFAULT 0,
    user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    session_id TEXT,
    denied BOOLEAN NOT NULL DEFAULT FALSE,
    kind TEXT NOT NULL CHECK (kind IN ('device','web')),
    redirect_uri TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT '',
    challenge TEXT NOT NULL DEFAULT ''
);
CREATE INDEX oauth_app_grants_expiry ON oauth_app_grants(expires_at);

CREATE TABLE oauth_app_tokens (
    token_id INTEGER PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL,
    scopes TEXT NOT NULL
);
CREATE INDEX oauth_app_tokens_client ON oauth_app_tokens(client_id);

-- tokens: fine-grained personal access tokens. Their personal_access_tokens row holds the hash,
-- user and expiry; this row marks the token fine-grained and holds its bounds. The owning
-- organization has no foreign key: deleting it must not turn the token into a classic one.
CREATE TABLE fine_grained_tokens (
    token_id INTEGER PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    owner_org_id INTEGER,
    description TEXT NOT NULL DEFAULT '' CHECK (length(description) <= 1024),
    repository_access TEXT NOT NULL CHECK (repository_access IN ('public','all','selected')),
    permissions TEXT NOT NULL CHECK (length(permissions) <= 4096),
    status TEXT NOT NULL CHECK (status IN ('pending','approved','denied','revoked')),
    reason TEXT CHECK (reason IS NULL OR length(reason) <= 1024),
    reviewed_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    reviewed_at TEXT,
    requested_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX fine_grained_tokens_org ON fine_grained_tokens(owner_org_id, status);
CREATE TABLE fine_grained_token_repositories (
    token_id INTEGER NOT NULL REFERENCES fine_grained_tokens(token_id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY (token_id, repo_id)
);
CREATE INDEX fine_grained_token_repositories_repo ON fine_grained_token_repositories(repo_id);
-- An organization without a row uses the defaults: fine-grained tokens allowed after owner
-- approval, classic tokens allowed, and a 366-day maximum. A NULL maximum allows no expiry.
CREATE TABLE organization_token_policies (
    org_id INTEGER PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    fine_grained_allowed INTEGER NOT NULL DEFAULT 1 CHECK (fine_grained_allowed IN (0,1)),
    fine_grained_approval INTEGER NOT NULL DEFAULT 1 CHECK (fine_grained_approval IN (0,1)),
    classic_allowed INTEGER NOT NULL DEFAULT 1 CHECK (classic_allowed IN (0,1)),
    max_lifetime_days INTEGER DEFAULT 366 CHECK (max_lifetime_days IS NULL OR max_lifetime_days BETWEEN 1 AND 366)
);

-- apps: GitHub Apps and installations
CREATE TABLE github_apps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    bot_id INTEGER NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL UNIQUE REFERENCES oauth_applications(client_id) ON DELETE CASCADE,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    homepage TEXT NOT NULL,
    public_key TEXT NOT NULL,
    permissions TEXT NOT NULL,
    events TEXT NOT NULL,
    access_level SMALLINT NOT NULL CHECK (access_level IN (1,3,5)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    is_public BOOLEAN NOT NULL DEFAULT 1 CHECK(is_public IN (0,1))
);

CREATE TABLE github_app_installations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES github_apps(id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE,
    all_repos BOOLEAN NOT NULL CHECK(all_repos IN (0,1)),
    suspended_at TEXT,
    suspended_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK ((user_id IS NULL) <> (org_id IS NULL)),
    UNIQUE (app_id,user_id), UNIQUE (app_id,org_id)
);

CREATE TABLE github_app_repositories (
    installation_id INTEGER NOT NULL REFERENCES github_app_installations(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(installation_id,repo_id)
);

CREATE TABLE github_app_tokens (
    token_id INTEGER PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    -- Retain credential bounds after an account/organization cascade removes its installation.
    -- Dropping this marker would turn a scoped user token back into an unscoped OAuth token.
    installation_id INTEGER NOT NULL,
    permissions TEXT NOT NULL,
    repositories TEXT NOT NULL,
    user_token BOOLEAN NOT NULL CHECK(user_token IN (0,1))
);
CREATE INDEX github_app_tokens_installation ON github_app_tokens(installation_id);

-- apps: account installation requests
CREATE TABLE github_app_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES github_apps(id) ON DELETE CASCADE,
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(app_id,org_id,user_id)
);

-- apps: webhook configuration and recorded attempts
CREATE TABLE github_app_hooks (
    app_id INTEGER PRIMARY KEY REFERENCES github_apps(id) ON DELETE CASCADE,
    payload_url TEXT NOT NULL CHECK(length(payload_url)<=2048),
    content_type TEXT NOT NULL CHECK(content_type IN ('application/json','application/x-www-form-urlencoded')),
    encrypted_secret BLOB,
    secret_nonce BLOB,
    events TEXT NOT NULL,
    active BOOLEAN NOT NULL CHECK(active IN (0,1)),
    ssl_verify BOOLEAN NOT NULL CHECK(ssl_verify IN (0,1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK((encrypted_secret IS NULL)=(secret_nonce IS NULL))
);

CREATE TABLE github_app_deliveries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id INTEGER NOT NULL REFERENCES github_app_hooks(app_id) ON DELETE CASCADE,
    guid TEXT NOT NULL,
    event TEXT NOT NULL,
    action TEXT,
    status_code INTEGER,
    duration_ms INTEGER NOT NULL,
    error TEXT,
    request_headers TEXT NOT NULL,
    request_body TEXT NOT NULL,
    response_headers TEXT,
    response_body TEXT,
    redelivery BOOLEAN NOT NULL CHECK(redelivery IN (0,1)),
    installation_id INTEGER,
    repository_id INTEGER,
    url TEXT NOT NULL,
    delivered_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX github_app_deliveries_hook ON github_app_deliveries(webhook_id,id);
CREATE INDEX github_app_deliveries_expiry ON github_app_deliveries(delivered_at, id);

CREATE TABLE github_app_callbacks (
    app_id INTEGER NOT NULL REFERENCES github_apps(id) ON DELETE CASCADE,
    uri TEXT NOT NULL,
    PRIMARY KEY(app_id,uri)
);

CREATE TABLE github_app_manifests (
    hash TEXT PRIMARY KEY,
    conversion_hash TEXT UNIQUE,
    manifest TEXT NOT NULL,
    state TEXT NOT NULL,
    user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL,
    CHECK((user_id IS NULL)=(session_id IS NULL))
);
CREATE INDEX github_app_manifests_expiry ON github_app_manifests(expires_at);

-- apps: installation identities cannot become interactive accounts
CREATE TRIGGER github_app_bot_identity BEFORE UPDATE ON users
WHEN EXISTS(SELECT 1 FROM github_apps WHERE bot_id=OLD.id)
 AND (NEW.username<>OLD.username OR NEW.is_admin OR NEW.password_hash IS NOT NULL OR NEW.email IS NOT NULL)
BEGIN SELECT RAISE(ABORT,'GitHub App bot identity is reserved'); END;
CREATE TRIGGER github_app_bot_session_insert BEFORE INSERT ON sessions
WHEN EXISTS(SELECT 1 FROM github_apps WHERE bot_id=NEW.user_id)
BEGIN SELECT RAISE(ABORT,'GitHub App bots cannot start browser sessions'); END;
CREATE TRIGGER github_app_bot_session_update BEFORE UPDATE OF user_id ON sessions
WHEN EXISTS(SELECT 1 FROM github_apps WHERE bot_id=NEW.user_id)
BEGIN SELECT RAISE(ABORT,'GitHub App bots cannot start browser sessions'); END;

-- deploy
CREATE TABLE deployment_environments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL,
    wait_timer INTEGER NOT NULL DEFAULT 0 CHECK (wait_timer BETWEEN 0 AND 43200),
    prevent_self_review BOOLEAN NOT NULL DEFAULT false,
    reviewers TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(reviewers) AND json_type(reviewers)='array' AND json_array_length(reviewers)<=6),
    branch_policy TEXT CHECK (branch_policy IN ('protected','custom')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(repo_id,name_key)
);

CREATE TABLE deployment_branch_policies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    environment_id INTEGER NOT NULL REFERENCES deployment_environments(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('branch','tag')),
    UNIQUE(environment_id,name,kind)
);

CREATE TABLE repository_deployments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    creator_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    sha TEXT NOT NULL,
    reference TEXT NOT NULL,
    task TEXT NOT NULL DEFAULT 'deploy',
    payload TEXT NOT NULL DEFAULT '{}',
    original_environment TEXT NOT NULL,
    environment TEXT NOT NULL,
    environment_key TEXT NOT NULL,
    description TEXT,
    transient_environment BOOLEAN NOT NULL DEFAULT false,
    production_environment BOOLEAN NOT NULL DEFAULT false,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX repository_deployments_repo ON repository_deployments(repo_id,id);
CREATE INDEX repository_deployments_inactivation ON repository_deployments(repo_id,environment_key,id) WHERE NOT transient_environment AND NOT production_environment;

CREATE TABLE deployment_statuses (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    deployment_id INTEGER NOT NULL REFERENCES repository_deployments(id) ON DELETE CASCADE,
    creator_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('error','failure','inactive','in_progress','queued','pending','success')),
    description TEXT NOT NULL DEFAULT '' CHECK (length(description)<=140),
    environment TEXT NOT NULL,
    log_url TEXT NOT NULL DEFAULT '',
    environment_url TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX deployment_statuses_deployment ON deployment_statuses(deployment_id,id);

-- packages
CREATE TABLE registry_namespaces (
    namespace TEXT PRIMARY KEY,
    owner_user_id INTEGER UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id INTEGER UNIQUE REFERENCES organizations(id) ON DELETE CASCADE,
    CHECK ((owner_user_id IS NOT NULL) <> (owner_org_id IS NOT NULL))
);

CREATE TABLE packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_kind TEXT NOT NULL CHECK (owner_kind IN ('user','org')),
    owner_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    package_type TEXT NOT NULL CHECK (package_type IN ('container','npm','maven','nuget','rubygems')),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('public','private')),
    repo_id INTEGER REFERENCES repositories(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    CHECK ((owner_kind='user' AND owner_user_id IS NOT NULL AND owner_org_id IS NULL) OR (owner_kind='org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL)),
    UNIQUE(owner_user_id,package_type,name),
    UNIQUE(owner_org_id,package_type,name)
);

CREATE TABLE package_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    digest TEXT NOT NULL,
    media_type TEXT NOT NULL,
    manifest TEXT NOT NULL,
    downloads INTEGER NOT NULL DEFAULT 0 CHECK (downloads>=0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    UNIQUE(package_id,name)
);

CREATE TABLE package_tags (
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    version_id INTEGER NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
    PRIMARY KEY(package_id,tag)
);
CREATE INDEX package_tags_version ON package_tags(version_id);

CREATE TABLE package_blobs (
    digest TEXT PRIMARY KEY,
    size INTEGER NOT NULL CHECK (size>=0),
    ready BOOLEAN NOT NULL DEFAULT false,
    deleting BOOLEAN NOT NULL DEFAULT false,
    expires_at INTEGER NOT NULL
);

CREATE TABLE package_blob_grants (
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    digest TEXT NOT NULL REFERENCES package_blobs(digest) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY(package_id,digest)
);

CREATE TABLE package_version_blobs (
    version_id INTEGER NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
    digest TEXT NOT NULL REFERENCES package_blobs(digest),
    PRIMARY KEY(version_id,digest)
);
CREATE INDEX package_version_blobs_digest ON package_version_blobs(digest);

CREATE TABLE package_files (
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    version_id INTEGER NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
    digest TEXT NOT NULL REFERENCES package_blobs(digest),
    PRIMARY KEY(package_id,path)
);
CREATE INDEX package_files_path ON package_files(path,package_id);
CREATE INDEX package_files_version ON package_files(version_id);

CREATE TABLE package_uploads (
    id TEXT PRIMARY KEY,
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    token_id INTEGER NOT NULL REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    uploaded_bytes INTEGER NOT NULL DEFAULT 0,
    chunks TEXT NOT NULL DEFAULT '[]',
    charge_id BLOB CHECK (charge_id IS NULL OR (typeof(charge_id)='blob' AND length(charge_id)=16)),
    expires_at INTEGER NOT NULL
);

CREATE TABLE package_registry_tokens (
    hash TEXT PRIMARY KEY,
    token_id INTEGER REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    resources TEXT NOT NULL,
    write BOOLEAN NOT NULL,
    expires_at INTEGER NOT NULL,
    CHECK (token_id IS NOT NULL OR NOT write)
);

-- projects
CREATE TABLE projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE,
    owner_kind TEXT GENERATED ALWAYS AS (CASE WHEN owner_org_id IS NULL THEN 'user' ELSE 'org' END) STORED,
    owner_id INTEGER GENERATED ALWAYS AS (COALESCE(owner_user_id, owner_org_id)) STORED,
    number INTEGER NOT NULL,
    creator_id INTEGER NOT NULL REFERENCES users(id),
    title TEXT NOT NULL,
    description TEXT,
    public BOOLEAN NOT NULL DEFAULT false,
    closed_at TEXT,
    deleted_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    readme TEXT NOT NULL DEFAULT '',
    template BOOLEAN NOT NULL DEFAULT false,
    CHECK ((owner_user_id IS NULL) <> (owner_org_id IS NULL)),
    UNIQUE(owner_kind, owner_id, number)
);

CREATE TABLE project_fields (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    data_type TEXT NOT NULL CHECK (data_type IN ('title','text','number','date','single_select','iteration')),
    configuration TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(project_id, name)
);
CREATE UNIQUE INDEX project_fields_name_folded ON project_fields(project_id, name COLLATE NOCASE);

-- migrations
CREATE TABLE repository_imports (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    generation TEXT NOT NULL,
    vcs_url TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('importing','complete','error')),
    updated_at BIGINT NOT NULL
);

CREATE TABLE pages_sites (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    build_type TEXT NOT NULL CHECK (build_type IN ('legacy','workflow')),
    branch TEXT, path TEXT CHECK (path IN ('/','/docs')), cname TEXT UNIQUE,
    https_enforced BOOLEAN NOT NULL DEFAULT FALSE, is_public BOOLEAN NOT NULL DEFAULT FALSE,
    CHECK ((branch IS NULL) = (path IS NULL))
);

CREATE TABLE pages_builds (
    id INTEGER PRIMARY KEY AUTOINCREMENT, repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL, commit_sha TEXT NOT NULL,
    status TEXT NOT NULL, error TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')), updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);
CREATE INDEX pages_builds_repo ON pages_builds(repo_id,id);

CREATE TABLE pages_deployments (
    id INTEGER PRIMARY KEY AUTOINCREMENT, repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL, environment TEXT NOT NULL, status TEXT NOT NULL,
    artifact_id BIGINT, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);
CREATE INDEX pages_deployments_repo ON pages_deployments(repo_id,id);

CREATE TABLE repository_private_reporting (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE CHECK (typeof(repo_id)='integer'),
    enabled_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE repository_security_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    reporter_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    title TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 255),
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65536),
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','closed')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX idx_security_reports_recent ON repository_security_reports(repo_id,id DESC);
CREATE INDEX idx_security_reports_reporter ON repository_security_reports(repo_id,reporter_id,created_at);

CREATE TABLE repository_security_replies (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (typeof(id)='integer'),
    report_id INTEGER NOT NULL REFERENCES repository_security_reports(id) ON DELETE CASCADE,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65536),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX idx_security_replies_report ON repository_security_replies(report_id,id);

CREATE TABLE repository_advisories (
    id INTEGER PRIMARY KEY AUTOINCREMENT, ghsa_id TEXT NOT NULL UNIQUE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('triage','draft','published','closed')),
    document TEXT NOT NULL,
    report_id INTEGER UNIQUE REFERENCES repository_security_reports(id) ON DELETE SET NULL,
    private_fork_id INTEGER UNIQUE REFERENCES repositories(id) ON DELETE SET NULL,
    cve_requested_at TEXT, published_at TEXT, closed_at TEXT,
    publisher_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    report_accepted BOOLEAN NOT NULL DEFAULT FALSE,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision)='integer' AND revision >= 0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')), updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);
CREATE INDEX repository_advisories_repo ON repository_advisories(repo_id,id);

-- dependabot
CREATE TABLE repo_dependabot_keys (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BLOB NOT NULL CHECK (length(encrypted_key)=48),
    nonce BLOB NOT NULL CHECK (length(nonce)=12)
);

CREATE TABLE repo_dependabot_secrets (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    encrypted_value BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce)=12),
    created_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(repo_id, name)
);

CREATE TABLE org_dependabot_keys (
    org_id INTEGER PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BLOB NOT NULL CHECK (length(encrypted_key)=48),
    nonce BLOB NOT NULL CHECK (length(nonce)=12)
);

CREATE TABLE org_dependabot_secrets (
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    visibility TEXT NOT NULL CHECK (visibility IN ('all','private','selected')),
    encrypted_value BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce)=12),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(org_id, name)
);

CREATE TABLE org_dependabot_secret_repositories (
    org_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    FOREIGN KEY(org_id, name) REFERENCES org_dependabot_secrets(org_id, name) ON DELETE CASCADE,
    PRIMARY KEY(org_id, name, repo_id)
);
CREATE INDEX org_dependabot_secret_repositories_repo ON org_dependabot_secret_repositories(repo_id);

-- Dependency graph
CREATE TABLE dependency_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    reference TEXT NOT NULL,
    correlator TEXT NOT NULL,
    detector TEXT NOT NULL,
    scanned TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX dependency_snapshots_reference ON dependency_snapshots(repo_id,reference,detector,correlator,scanned DESC,id DESC);
CREATE INDEX dependency_snapshots_head ON dependency_snapshots(repo_id,reference,id DESC,sha);
CREATE INDEX dependency_snapshots_sha ON dependency_snapshots(repo_id,sha,detector,correlator,scanned DESC,id DESC);

CREATE TABLE dependency_sbom_reports (
    id TEXT PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX dependency_sbom_reports_repository ON dependency_sbom_reports(repo_id,created_at);

-- actions-admin: organization and environment values.
CREATE TABLE org_actions_keys (
    org_id INTEGER PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BLOB NOT NULL CHECK (length(encrypted_key)=48),
    nonce BLOB NOT NULL CHECK (length(nonce)=12)
);

CREATE TABLE org_actions_values (
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('secret','variable')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255 AND name=upper(name)),
    value TEXT NOT NULL DEFAULT '' CHECK (length(CAST(value AS BLOB))<=49152),
    encrypted_value BLOB,
    nonce BLOB,
    visibility TEXT NOT NULL CHECK (visibility IN ('all','private','selected')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(org_id,kind,name),
    CHECK ((kind='secret' AND value='' AND encrypted_value IS NOT NULL AND length(encrypted_value) BETWEEN 16 AND 49168 AND nonce IS NOT NULL AND length(nonce)=12) OR (kind='variable' AND encrypted_value IS NULL AND nonce IS NULL))
);

CREATE TABLE org_actions_value_repositories (
    org_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(org_id,kind,name,repo_id),
    FOREIGN KEY(org_id,kind,name) REFERENCES org_actions_values(org_id,kind,name) ON DELETE CASCADE ON UPDATE CASCADE
);
CREATE INDEX org_actions_value_repo ON org_actions_value_repositories(repo_id);

CREATE TABLE repo_environment_values (
    repo_id INTEGER NOT NULL,
    environment TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('secret','variable')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255 AND name=upper(name)),
    value TEXT NOT NULL DEFAULT '' CHECK (length(CAST(value AS BLOB))<=49152),
    encrypted_value BLOB,
    nonce BLOB,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(repo_id,environment,kind,name),
    FOREIGN KEY(repo_id,environment) REFERENCES deployment_environments(repo_id,name_key) ON DELETE CASCADE ON UPDATE CASCADE,
    CHECK ((kind='secret' AND value='' AND encrypted_value IS NOT NULL AND length(encrypted_value) BETWEEN 16 AND 49168 AND nonce IS NOT NULL AND length(nonce)=12) OR (kind='variable' AND encrypted_value IS NULL AND nonce IS NULL))
);

-- actions-admin: signed attestation evidence, not a trust-verification verdict.
CREATE TABLE attestation_bundles (
    hash TEXT PRIMARY KEY,
    bundle TEXT NOT NULL CHECK (json_valid(bundle) AND json_type(bundle)='object' AND length(CAST(bundle AS BLOB)) BETWEEN 1 AND 1048576)
);

CREATE TABLE artifact_attestations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    bundle_hash TEXT NOT NULL REFERENCES attestation_bundles(hash),
    predicate_type TEXT NOT NULL CHECK (length(CAST(predicate_type AS BLOB)) BETWEEN 1 AND 2048),
    initiator TEXT NOT NULL CHECK (initiator IN ('user','workflow')),
    created_at TEXT NOT NULL,
    UNIQUE(repo_id,bundle_hash)
);

CREATE TABLE attestation_subjects (
    attestation_id INTEGER NOT NULL REFERENCES artifact_attestations(id) ON DELETE CASCADE,
    digest TEXT NOT NULL CHECK (((length(digest)=71 AND substr(digest,1,7)='sha256:') OR (length(digest)=135 AND substr(digest,1,7)='sha512:')) AND substr(digest,8) NOT GLOB '*[^0-9a-f]*'),
    PRIMARY KEY(attestation_id,digest)
);
CREATE INDEX attestation_subject_digest ON attestation_subjects(digest,attestation_id);
CREATE INDEX attestation_bundle_references ON artifact_attestations(bundle_hash);
CREATE VIEW repo_attestations AS SELECT a.id,a.repo_id,b.bundle,a.predicate_type,a.initiator,a.created_at FROM artifact_attestations a JOIN attestation_bundles b ON b.hash=a.bundle_hash;

CREATE TABLE artifact_metadata (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    digest TEXT NOT NULL CHECK (length(digest)=71 AND substr(digest,1,7)='sha256:'),
    name TEXT NOT NULL,
    version TEXT,
    UNIQUE(repo_id,digest)
);
CREATE INDEX artifact_metadata_digest ON artifact_metadata(digest,repo_id);

CREATE TABLE artifact_metadata_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    artifact_id INTEGER NOT NULL REFERENCES artifact_metadata(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('deployment','storage')),
    identity TEXT NOT NULL,
    cluster TEXT NOT NULL DEFAULT '',
    logical_environment TEXT NOT NULL DEFAULT '',
    physical_environment TEXT NOT NULL DEFAULT '',
    document TEXT NOT NULL,
    attestation_id INTEGER REFERENCES artifact_attestations(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(org_id,kind,identity)
);
CREATE INDEX artifact_metadata_record_artifact ON artifact_metadata_records(artifact_id,kind,id);
CREATE INDEX artifact_metadata_record_cluster ON artifact_metadata_records(org_id,cluster,logical_environment,physical_environment) WHERE kind='deployment';
CREATE INDEX artifact_metadata_record_attestation ON artifact_metadata_records(attestation_id) WHERE attestation_id IS NOT NULL;

CREATE TABLE artifact_metadata_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    cluster TEXT NOT NULL,
    actor_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_id INTEGER NOT NULL,
    scoped_repo INTEGER REFERENCES repositories(id) ON DELETE CASCADE,
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','processing','completed','failed')),
    started_at TEXT,
    completed_at TEXT,
    total_count INTEGER,
    errors TEXT NOT NULL DEFAULT '[]',
    CHECK ((status IN ('completed','failed')) = (completed_at IS NOT NULL)),
    CHECK (status NOT IN ('completed','failed') OR input='')
);
CREATE UNIQUE INDEX artifact_metadata_active_job ON artifact_metadata_jobs(org_id,cluster) WHERE status IN ('pending','processing');
CREATE INDEX artifact_metadata_pending_jobs ON artifact_metadata_jobs(id) WHERE status='pending';
CREATE INDEX artifact_metadata_job_expiry ON artifact_metadata_jobs(completed_at,id) WHERE status IN ('completed','failed');
CREATE INDEX artifact_metadata_job_history ON artifact_metadata_jobs(actor_id,org_id,scoped_repo,id) WHERE status IN ('completed','failed');

-- code-scanning
CREATE TABLE code_scanning_uploads (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    ref_name TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    sarif TEXT NOT NULL,
    checkout_uri TEXT,
    processing_status TEXT NOT NULL DEFAULT 'pending' CHECK(processing_status IN ('pending','complete','failed')),
    error TEXT,
    created_at TEXT NOT NULL,
    claim_id TEXT,
    claim_until TEXT
);
CREATE INDEX code_scanning_uploads_pending ON code_scanning_uploads(processing_status,sequence);

CREATE TABLE code_scanning_analyses (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    upload_id TEXT NOT NULL REFERENCES code_scanning_uploads(id) ON DELETE CASCADE,
    upload_sequence BIGINT NOT NULL,
    ref_name TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    tool_key TEXT NOT NULL,
    tool TEXT NOT NULL,
    category TEXT NOT NULL,
    results_count BIGINT NOT NULL,
    rules_count BIGINT NOT NULL,
    sarif TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(upload_id,tool_key,category)
);
CREATE INDEX code_scanning_analyses_repository ON code_scanning_analyses(repo_id,id);

CREATE TABLE code_scanning_alerts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number BIGINT NOT NULL,
    fingerprint TEXT NOT NULL,
    tool_key TEXT NOT NULL,
    tool TEXT NOT NULL,
    rule TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open','fixed','dismissed')),
    dismissed_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    dismissed_at TEXT,
    dismissed_reason TEXT,
    dismissed_comment TEXT,
    fixed_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(repo_id,number),
    UNIQUE(repo_id,tool_key,fingerprint)
);

CREATE TABLE code_scanning_instances (
    alert_id BIGINT NOT NULL REFERENCES code_scanning_alerts(id) ON DELETE CASCADE,
    ref_name TEXT NOT NULL,
    category TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('open','fixed')),
    message TEXT NOT NULL,
    location TEXT NOT NULL,
    upload_sequence BIGINT NOT NULL,
    PRIMARY KEY(alert_id,ref_name,category)
);

CREATE TABLE code_security_configurations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    settings TEXT NOT NULL,
    default_for_new_repos TEXT NOT NULL DEFAULT 'none' CHECK(default_for_new_repos IN ('all','none','public','private_and_internal')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(org_id,name)
);

CREATE INDEX code_security_configurations_page ON code_security_configurations(org_id,id DESC);

CREATE TABLE repository_security_settings (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    configuration_id BIGINT REFERENCES code_security_configurations(id) ON DELETE SET NULL,
    security_and_analysis TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX repository_security_configuration ON repository_security_settings(configuration_id);

CREATE TABLE code_scanning_alert_counters (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    next_number BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE code_scanning_analysis_findings (
    analysis_id BIGINT NOT NULL REFERENCES code_scanning_analyses(id) ON DELETE CASCADE,
    alert_id BIGINT NOT NULL REFERENCES code_scanning_alerts(id) ON DELETE CASCADE,
    result_index BIGINT NOT NULL,
    rule TEXT NOT NULL,
    message TEXT NOT NULL,
    location TEXT NOT NULL,
    PRIMARY KEY(analysis_id,alert_id)
);
CREATE INDEX code_scanning_findings_alert ON code_scanning_analysis_findings(alert_id);
CREATE INDEX code_scanning_uploads_actor ON code_scanning_uploads(actor_id,created_at);
CREATE INDEX code_scanning_instances_upload ON code_scanning_instances(upload_sequence,alert_id);
CREATE INDEX code_scanning_alerts_created ON code_scanning_alerts(repo_id,created_at DESC,id DESC);
CREATE INDEX code_scanning_alerts_updated ON code_scanning_alerts(repo_id,updated_at DESC,id DESC);
CREATE INDEX code_scanning_analyses_stream ON code_scanning_analyses(repo_id,ref_name,tool_key,category,upload_sequence DESC,id DESC);
CREATE INDEX code_scanning_analyses_revision ON code_scanning_analyses(repo_id,commit_sha,tool_key,category,upload_sequence DESC,id DESC);
CREATE INDEX code_scanning_instances_recent ON code_scanning_instances(alert_id,upload_sequence DESC,ref_name,category);
CREATE INDEX code_scanning_instances_open ON code_scanning_instances(alert_id,(CASE WHEN state='open' THEN 0 ELSE 1 END),upload_sequence DESC,ref_name,category);
CREATE UNIQUE INDEX organization_invitations_email ON organization_invitations(org_id,lower(email)) WHERE email IS NOT NULL;

CREATE TABLE organization_member_history (
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    removed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(org_id,user_id)
);
CREATE UNIQUE INDEX email_outbox_organization_invitation ON email_outbox(organization_invitation_id) WHERE organization_invitation_id IS NOT NULL;

CREATE TABLE organization_blocks (
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK (typeof(org_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(org_id,user_id)
);
CREATE INDEX organization_blocks_user ON organization_blocks(user_id);

CREATE TABLE organization_roles (
    id INTEGER PRIMARY KEY CHECK (typeof(id)='integer' AND id>0),
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    base_level INTEGER NOT NULL CHECK (typeof(base_level)='integer' AND base_level BETWEEN 1 AND 5),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
INSERT INTO organization_roles(id,name,description,base_level) VALUES
    (1,'All-repository read','Read every repository in the organization.',1),
    (2,'All-repository triage','Triage issues and pull requests in every repository.',2),
    (3,'All-repository write','Write to every repository in the organization.',3),
    (4,'All-repository maintain','Maintain every repository in the organization.',4),
    (5,'All-repository admin','Administer every repository without organization ownership.',5),
(6,'Security manager','Read every repository and manage organization repository security without organization ownership.',1);
CREATE UNIQUE INDEX organization_teams_owner_id ON teams(org_id,id);

CREATE TABLE organization_role_assignments (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK (typeof(org_id)='integer'),
    role_id INTEGER NOT NULL REFERENCES organization_roles(id) CHECK (typeof(role_id)='integer'),
    user_id INTEGER CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    team_id INTEGER CHECK (team_id IS NULL OR typeof(team_id)='integer'),
    CHECK ((user_id IS NULL) <> (team_id IS NULL)),
    FOREIGN KEY(org_id,user_id) REFERENCES org_memberships(org_id,user_id) ON DELETE CASCADE,
    FOREIGN KEY(org_id,team_id) REFERENCES teams(org_id,id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX organization_role_users ON organization_role_assignments(org_id,user_id,role_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX organization_role_teams ON organization_role_assignments(org_id,team_id,role_id) WHERE team_id IS NOT NULL;
CREATE INDEX organization_role_members ON organization_role_assignments(org_id,role_id);

-- Organization repository property definitions and tenant-scoped explicit values.
CREATE TABLE organization_repository_properties (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK(typeof(org_id)='integer'),
    property_name TEXT NOT NULL CHECK(length(property_name) BETWEEN 1 AND 75 AND property_name NOT GLOB '*[^a-zA-Z0-9_-]*'),
    value_type TEXT NOT NULL CHECK(value_type IN ('string','single_select','multi_select','true_false','url')),
    required BOOLEAN NOT NULL DEFAULT 0 CHECK(typeof(required)='integer' AND required IN (0,1)),
    default_value TEXT CHECK(default_value IS NULL OR (json_valid(default_value) AND json_type(default_value) IN ('text','array'))),
    description TEXT,
    allowed_values TEXT CHECK(allowed_values IS NULL OR (json_valid(allowed_values) AND json_type(allowed_values)='array')),
    values_editable_by TEXT NOT NULL DEFAULT 'org_actors' CHECK(values_editable_by IN ('org_actors','org_and_repo_actors')),
    require_explicit_values BOOLEAN NOT NULL DEFAULT 0 CHECK(typeof(require_explicit_values)='integer' AND require_explicit_values IN (0,1)),
    CHECK(value_type IN ('single_select','multi_select') OR allowed_values IS NULL),
    UNIQUE(org_id,id)
);
CREATE UNIQUE INDEX organization_repository_property_names ON organization_repository_properties(org_id,LOWER(property_name));
CREATE UNIQUE INDEX repository_properties_owner_id ON repositories(owner_org_id,id);

CREATE TABLE repository_property_values (
    org_id INTEGER NOT NULL CHECK(typeof(org_id)='integer'),
    repo_id INTEGER NOT NULL CHECK(typeof(repo_id)='integer'),
    definition_id INTEGER NOT NULL CHECK(typeof(definition_id)='integer'),
    value TEXT NOT NULL CHECK(json_valid(value) AND json_type(value) IN ('text','array')),
    PRIMARY KEY(repo_id,definition_id),
    FOREIGN KEY(org_id,repo_id) REFERENCES repositories(owner_org_id,id) ON DELETE CASCADE,
    FOREIGN KEY(org_id,definition_id) REFERENCES organization_repository_properties(org_id,id) ON DELETE CASCADE
);
CREATE INDEX repository_property_definition_values ON repository_property_values(definition_id,repo_id);

CREATE TABLE organization_settings (
    org_id INTEGER PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE CHECK(typeof(org_id)='integer'),
    company TEXT,
    email TEXT,
    twitter_username TEXT,
    location TEXT,
    blog TEXT,
    has_organization_projects INTEGER NOT NULL DEFAULT 1 CHECK(typeof(has_organization_projects)='integer' AND has_organization_projects IN (0,1)),
    default_repository_permission INTEGER NOT NULL DEFAULT 0 CHECK(typeof(default_repository_permission)='integer' AND default_repository_permission IN (0,1,3,5)),
    members_can_create_public_repositories INTEGER NOT NULL DEFAULT 0 CHECK(typeof(members_can_create_public_repositories)='integer' AND members_can_create_public_repositories IN (0,1)),
    members_can_create_private_repositories INTEGER NOT NULL DEFAULT 0 CHECK(typeof(members_can_create_private_repositories)='integer' AND members_can_create_private_repositories IN (0,1)),
    members_can_fork_private_repositories INTEGER NOT NULL DEFAULT 1 CHECK(typeof(members_can_fork_private_repositories)='integer' AND members_can_fork_private_repositories IN (0,1)),
    web_commit_signoff_required INTEGER NOT NULL DEFAULT 0 CHECK(typeof(web_commit_signoff_required)='integer' AND web_commit_signoff_required IN (0,1)),
    deploy_keys_enabled_for_repositories INTEGER NOT NULL DEFAULT 1 CHECK(typeof(deploy_keys_enabled_for_repositories)='integer' AND deploy_keys_enabled_for_repositories IN (0,1))
);

-- pulls: team identities remain requests as membership changes.
CREATE TABLE pr_team_reviewers (
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    as_code_owner INTEGER NOT NULL DEFAULT 0 CHECK (as_code_owner IN (0,1)),
    PRIMARY KEY(pull_request_id, team_id)
);
CREATE INDEX pr_team_reviewers_team ON pr_team_reviewers(team_id);

CREATE TABLE repository_exports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guid TEXT NOT NULL UNIQUE,
    owner_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    repositories TEXT NOT NULL CHECK (json_valid(repositories) AND json_type(repositories)='array'),
    options TEXT NOT NULL CHECK (json_valid(options) AND json_type(options)='object'),
    state TEXT NOT NULL CHECK (state IN ('pending','exporting','exported','failed')),
    archive_deleted BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    CHECK ((owner_user_id IS NOT NULL) <> (owner_org_id IS NOT NULL))
);

CREATE TABLE repository_migration_locks (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    migration_id INTEGER NOT NULL REFERENCES repository_exports(id) ON DELETE CASCADE
);
CREATE INDEX repository_exports_user ON repository_exports(owner_user_id,id);
CREATE INDEX repository_exports_active_actor ON repository_exports(actor_id, updated_at) WHERE state IN ('pending','exporting');
CREATE INDEX repository_exports_org ON repository_exports(owner_org_id,id);
CREATE TRIGGER repository_exports_cleanup BEFORE DELETE ON repository_exports
BEGIN
    INSERT INTO pending_object_deletions(object_key,delete_after)
    VALUES('migrations/' || OLD.guid,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+1 hour'))
    ON CONFLICT(object_key) DO UPDATE SET delete_after=MAX(delete_after,excluded.delete_after);
END;

CREATE TABLE project_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    creator_id INTEGER NOT NULL REFERENCES users(id),
    repo_id INTEGER REFERENCES repositories(id) ON DELETE CASCADE,
    issue_id INTEGER REFERENCES issues(id) ON DELETE CASCADE,
    pull_request_id INTEGER REFERENCES pull_requests(id) ON DELETE CASCADE,
    draft_title TEXT,
    draft_body TEXT,
    field_values TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    archived BOOLEAN NOT NULL DEFAULT false,
    archived_at TEXT,
    position INTEGER NOT NULL DEFAULT 0,
    CHECK (archived = (archived_at IS NOT NULL)),
    CHECK ((issue_id IS NOT NULL AND pull_request_id IS NULL AND draft_title IS NULL AND repo_id IS NOT NULL)
        OR (pull_request_id IS NOT NULL AND issue_id IS NULL AND draft_title IS NULL AND repo_id IS NOT NULL)
        OR (draft_title IS NOT NULL AND issue_id IS NULL AND pull_request_id IS NULL AND repo_id IS NULL)),
    UNIQUE(project_id, issue_id),
    UNIQUE(project_id, pull_request_id)
);

CREATE TABLE project_views (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    creator_id INTEGER NOT NULL REFERENCES users(id),
    configuration TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(project_id, number)
);

CREATE TABLE project_repositories (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(project_id, repo_id)
);

CREATE TABLE github_api_limits (
    requester TEXT NOT NULL,
    resource TEXT NOT NULL CHECK (resource IN ('core','search')),
    used INTEGER NOT NULL CHECK (used>=0),
    reset_at INTEGER NOT NULL,
    PRIMARY KEY(requester,resource)
);
CREATE INDEX github_api_limits_expiry ON github_api_limits(reset_at);

-- secret-scanning
CREATE TABLE secret_scanning_settings (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT false,
    push_protection BOOLEAN NOT NULL DEFAULT false,
    next_number BIGINT NOT NULL DEFAULT 1,
    CHECK (NOT push_protection OR enabled)
);

CREATE TABLE secret_scanning_alerts (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number BIGINT NOT NULL,
    secret_hash TEXT NOT NULL,
    secret_type TEXT NOT NULL,
    display_name TEXT NOT NULL,
    encrypted_secret BLOB NOT NULL,
    nonce BLOB NOT NULL,
    assigned_to BIGINT REFERENCES users(id) ON DELETE SET NULL,
    validity TEXT CHECK (validity IN ('active','inactive')),
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','resolved')),
    resolution TEXT CHECK (resolution IN ('false_positive','wont_fix','revoked','used_in_tests','pattern_deleted','pattern_edited')),
    resolution_comment TEXT,
    resolved_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    resolved_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY (repo_id, number),
    UNIQUE (repo_id, secret_hash, secret_type),
    CHECK ((state='open' AND resolution IS NULL) OR (state='resolved' AND resolution IS NOT NULL))
);

CREATE TABLE secret_scanning_locations (
    repo_id BIGINT NOT NULL,
    alert_number BIGINT NOT NULL,
    commit_sha TEXT NOT NULL,
    blob_sha TEXT NOT NULL,
    path TEXT NOT NULL,
    -- SHA-256 of the path, the key column. PostgreSQL needs it because a long path can
    -- exceed a B-tree entry; SQLite keeps the same key so both engines copy alike.
    path_digest TEXT NOT NULL,
    start_line BIGINT NOT NULL,
    end_line BIGINT NOT NULL,
    start_column BIGINT NOT NULL,
    end_column BIGINT NOT NULL,
    PRIMARY KEY (repo_id, alert_number, commit_sha, path_digest, start_line, start_column),
    FOREIGN KEY (repo_id, alert_number) REFERENCES secret_scanning_alerts(repo_id, number) ON DELETE CASCADE
);

CREATE TABLE secret_scanning_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    head_sha TEXT,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    scan_type TEXT NOT NULL CHECK (scan_type IN ('backfill','incremental','pattern_update','custom_pattern_backfill')),
    pattern_name TEXT,
    pattern_scope TEXT CHECK (pattern_scope IN ('repository','organization')),
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','completed','failed')),
    -- Counts claims, so a worker whose claim expired cannot report over its replacement.
    attempt BIGINT NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    started_at TEXT,
    heartbeat_at TEXT,
    completed_at TEXT
);
CREATE INDEX secret_scanning_jobs_pending ON secret_scanning_jobs(status, id);
CREATE INDEX secret_scanning_jobs_history ON secret_scanning_jobs(repo_id, id DESC);

CREATE TABLE secret_scanning_commits (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL,
    -- TRUE once the commit's ancestors were scanned too, not only its own tree.
    ancestry_complete BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (repo_id, commit_sha)
);

CREATE TABLE secret_scanning_bypasses (
    id TEXT PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL,
    token_type TEXT NOT NULL,
    reason TEXT CHECK (reason IN ('false_positive','used_in_tests','will_fix_later')),
    expires_at TEXT NOT NULL,
    used_at TEXT
);
CREATE INDEX secret_scanning_bypasses_value ON secret_scanning_bypasses(repo_id, actor_id, secret_hash);
CREATE INDEX secret_scanning_bypasses_unused_expiry ON secret_scanning_bypasses(expires_at, id) WHERE used_at IS NULL;

CREATE TABLE secret_scanning_patterns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    document TEXT NOT NULL,
    CHECK ((repo_id IS NULL) <> (org_id IS NULL)),
    UNIQUE(repo_id, name),
    UNIQUE(org_id, name)
);

CREATE TABLE secret_scanning_pattern_configs (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    document TEXT NOT NULL
);

-- Repository feature switches with GitHub's defaults; a repository without a row has them all.
CREATE TABLE repository_features (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    wiki_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    has_issues BOOLEAN NOT NULL DEFAULT TRUE,
    has_projects BOOLEAN NOT NULL DEFAULT TRUE,
    allow_forking BOOLEAN NOT NULL DEFAULT TRUE,
    delete_branch_on_merge BOOLEAN NOT NULL DEFAULT FALSE,
    allow_update_branch BOOLEAN NOT NULL DEFAULT FALSE,
    allow_auto_merge BOOLEAN NOT NULL DEFAULT FALSE,
    squash_merge_commit_title TEXT NOT NULL DEFAULT 'COMMIT_OR_PR_TITLE' CHECK (squash_merge_commit_title IN ('PR_TITLE','COMMIT_OR_PR_TITLE')),
    squash_merge_commit_message TEXT NOT NULL DEFAULT 'COMMIT_MESSAGES' CHECK (squash_merge_commit_message IN ('PR_BODY','COMMIT_MESSAGES','BLANK'))
);

-- discussions
CREATE TABLE repository_discussion_settings (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT false
);

CREATE TABLE discussion_categories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    emoji TEXT NOT NULL,
    description TEXT NOT NULL,
    answerable BOOLEAN NOT NULL DEFAULT false,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(repo_id,slug),
    UNIQUE(id,repo_id)
);

CREATE TABLE discussions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    author_id BIGINT NOT NULL REFERENCES users(id),
    category_id BIGINT NOT NULL,
    locked BOOLEAN NOT NULL DEFAULT false,
    answer_id BIGINT REFERENCES discussion_comments(id) ON DELETE SET NULL,
    answer_chosen_at TEXT,
    answer_chosen_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    FOREIGN KEY(category_id,repo_id) REFERENCES discussion_categories(id,repo_id),
    UNIQUE(repo_id,number)
);

CREATE TABLE discussion_comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    discussion_id BIGINT NOT NULL REFERENCES discussions(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL REFERENCES users(id),
    body TEXT NOT NULL,
    reply_to_id BIGINT REFERENCES discussion_comments(id) ON DELETE CASCADE,
    is_answer BOOLEAN NOT NULL DEFAULT false,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX discussion_comments_thread ON discussion_comments(discussion_id,reply_to_id,id);
CREATE UNIQUE INDEX discussion_one_answer ON discussion_comments(discussion_id) WHERE is_answer;

CREATE TABLE discussion_labels (
    discussion_id BIGINT NOT NULL REFERENCES discussions(id) ON DELETE CASCADE,
    label_id BIGINT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY(discussion_id,label_id)
);

-- vulnerability scanning
CREATE TABLE vulnerability_settings (
    id INTEGER PRIMARY KEY CHECK(id=1),
    base_url TEXT NOT NULL DEFAULT 'https://osv-vulnerabilities.storage.googleapis.com',
    ecosystems TEXT NOT NULL DEFAULT '[]',
    next_sync TEXT NOT NULL DEFAULT '1970-01-01T00:00:00.000000Z'
);
INSERT INTO vulnerability_settings(id) VALUES(1);

CREATE TABLE osv_advisories (
    id TEXT PRIMARY KEY,
    modified TEXT NOT NULL,
    data TEXT NOT NULL
);

CREATE TABLE osv_packages (
    advisory TEXT NOT NULL REFERENCES osv_advisories(id) ON DELETE CASCADE,
    ecosystem TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY(advisory,ecosystem,name)
);
CREATE INDEX osv_packages_lookup ON osv_packages(ecosystem,name);

CREATE TABLE vulnerability_jobs (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    next_scan TEXT NOT NULL,
    lease_until TEXT
);
CREATE INDEX vulnerability_jobs_due ON vulnerability_jobs(next_scan, repo_id);
CREATE TRIGGER repository_vulnerability_job AFTER INSERT ON repositories WHEN NOT NEW.is_gist
BEGIN
    INSERT INTO vulnerability_jobs(repo_id,generation,next_scan) VALUES(NEW.id,1,strftime('%Y-%m-%dT%H:%M:%f000Z','now'));
END;

-- Actions job annotations
CREATE TABLE actions_job_annotations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES actions_jobs(id) ON DELETE CASCADE,
    annotation TEXT NOT NULL
);
CREATE INDEX actions_job_annotations_job ON actions_job_annotations(job_id,id);

-- CodeQL databases
CREATE TABLE codeql_databases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    language TEXT NOT NULL,
    name TEXT NOT NULL,
    uploader_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    object_key TEXT NOT NULL UNIQUE,
    size BIGINT NOT NULL CHECK (size > 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(repo_id, language)
);
CREATE TRIGGER codeql_database_delete AFTER DELETE ON codeql_databases BEGIN
    INSERT INTO pending_object_deletions(object_key,delete_after)
    VALUES(OLD.object_key,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+7 days'))
    ON CONFLICT(object_key) DO NOTHING;
END;
CREATE TRIGGER codeql_database_replace AFTER UPDATE OF object_key ON codeql_databases
WHEN OLD.object_key <> NEW.object_key BEGIN
    INSERT INTO pending_object_deletions(object_key,delete_after)
    VALUES(OLD.object_key,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+7 days'))
    ON CONFLICT(object_key) DO NOTHING;
END;

-- Organization settings and migration authors
CREATE TABLE organization_immutable_releases (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    enforced_repositories TEXT NOT NULL CHECK (enforced_repositories IN ('all','selected','none'))
);

CREATE TABLE organization_immutable_release_repositories (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY (org_id, repo_id)
);

CREATE TABLE organization_external_property_installations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    installation_id BIGINT NOT NULL REFERENCES github_app_installations(id) ON DELETE CASCADE,
    UNIQUE (org_id, display_name),
    UNIQUE (org_id, installation_id)
);

CREATE TABLE organization_external_property_values (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    property_name TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (org_id, repo_id, property_name)
);

CREATE TABLE repository_import_authors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    author_name TEXT NOT NULL,
    author_email TEXT NOT NULL,
    identity_hash TEXT NOT NULL CHECK(length(identity_hash)=64),
    mapped_name TEXT,
    mapped_email TEXT,
    UNIQUE (repo_id, identity_hash)
);

CREATE TABLE project_teams (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    PRIMARY KEY(project_id, team_id)
);

-- checks: publisher-owned runs, annotations and repository preferences.
CREATE TABLE check_suites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    publisher TEXT NOT NULL,
    app_id INTEGER,
    -- A workflow run's suite; its status and conclusion are read from the run.
    ci_run_id INTEGER UNIQUE REFERENCES ci_runs(id) ON DELETE CASCADE,
    head_sha TEXT NOT NULL,
    head_branch TEXT,
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','in_progress','completed')),
    conclusion TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(repo_id,publisher,head_sha)
);
CREATE INDEX check_suites_ref ON check_suites(repo_id,head_sha,id);

CREATE TABLE check_runs (
    id BIGINT PRIMARY KEY NOT NULL REFERENCES check_run_identities(id),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    suite_id INTEGER NOT NULL REFERENCES check_suites(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','in_progress','completed')),
    conclusion TEXT,
    data TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX check_runs_latest ON check_runs(suite_id,name,id);
CREATE INDEX check_runs_repo ON check_runs(repo_id,id);
CREATE INDEX check_runs_publisher_latest ON check_runs(repo_id,name,id);

CREATE TABLE check_annotations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id INTEGER NOT NULL REFERENCES check_runs(id) ON DELETE CASCADE,
    data TEXT NOT NULL
);
CREATE INDEX check_annotations_run ON check_annotations(run_id,id);

CREATE TABLE check_suite_preferences (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    app_id INTEGER NOT NULL REFERENCES github_apps(id) ON DELETE CASCADE,
    setting INTEGER NOT NULL,
    PRIMARY KEY(repo_id,app_id)
);

CREATE TABLE actions_policies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK ((repo_id IS NULL) <> (org_id IS NULL))
);
CREATE INDEX actions_policies_repo ON actions_policies(repo_id);
CREATE INDEX actions_policies_org ON actions_policies(org_id);

CREATE TABLE actions_deployment_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    -- Historical identity: deleting the live environment must not delete an approval.
    environment_id BIGINT NOT NULL,
    environment_name TEXT NOT NULL,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('approved','rejected')),
    comment TEXT NOT NULL,
    deployment_id BIGINT REFERENCES repository_deployments(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(run_id,environment_id)
);

-- repos: collaborator invitations.
CREATE TABLE repository_invitations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    invitee_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    inviter_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    permission SMALLINT NOT NULL CHECK (permission BETWEEN 1 AND 5),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','accepted','declined','cancelled','expired')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE UNIQUE INDEX repository_invitations_pending ON repository_invitations(repo_id,invitee_id) WHERE state='pending';
CREATE INDEX repository_invitations_recipient ON repository_invitations(invitee_id,state,created_at);
CREATE INDEX repository_invitations_daily ON repository_invitations(repo_id,created_at);

-- Lookup only: releases can hold several forks of one source in an organization, so creation
-- and transfer refuse a second one inside their write transaction.
CREATE INDEX idx_repositories_org_fork_source ON repositories(owner_org_id,forked_from_id) WHERE owner_kind='org' AND forked_from_id IS NOT NULL;

CREATE TABLE repository_autolinks (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (typeof(repo_id)='integer'),
    key_prefix TEXT NOT NULL CHECK (length(key_prefix) BETWEEN 1 AND 80),
    url_template TEXT NOT NULL CHECK (length(url_template) BETWEEN 1 AND 4096),
    is_alphanumeric INTEGER NOT NULL DEFAULT 1 CHECK (is_alphanumeric IN (0,1)),
    UNIQUE(repo_id,key_prefix)
);

CREATE TABLE repository_ref_activities (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    operation_id TEXT NOT NULL CHECK (length(operation_id)=36),
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 1024),
    before_sha TEXT NOT NULL CHECK (length(before_sha)=40),
    after_sha TEXT NOT NULL CHECK (length(after_sha)=40),
    activity_type TEXT NOT NULL CHECK (activity_type IN ('push','force_push','branch_creation','branch_deletion','pr_merge','merge_queue_merge')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(repo_id,operation_id,reference)
);
CREATE INDEX idx_ref_activities_recent ON repository_ref_activities(repo_id,id DESC);
CREATE INDEX idx_ref_activities_reference ON repository_ref_activities(repo_id,reference,id);
CREATE INDEX idx_ref_activities_actor ON repository_ref_activities(repo_id,actor_id,id);

CREATE TABLE repository_traffic_keys (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day TEXT NOT NULL CHECK(length(day)=10),
    salt TEXT NOT NULL CHECK(length(salt)=64),
    PRIMARY KEY(repo_id,day)
);

CREATE TABLE repository_traffic_visitors (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day TEXT NOT NULL CHECK(length(day)=10),
    fingerprint TEXT NOT NULL CHECK(length(fingerprint)=64),
    visitor TEXT NOT NULL,
    PRIMARY KEY(repo_id,day,fingerprint)
);

CREATE TABLE repository_traffic_counts (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day TEXT NOT NULL CHECK(length(day)=10),
    kind TEXT NOT NULL CHECK(kind IN ('view','clone')),
    visitor TEXT NOT NULL,
    path_hash TEXT NOT NULL CHECK(length(path_hash)=64),
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    referrer TEXT NOT NULL,
    count INTEGER NOT NULL CHECK(count>0),
    last_seen TEXT NOT NULL,
    PRIMARY KEY(repo_id,day,kind,visitor,path_hash,referrer)
);
CREATE INDEX idx_repository_traffic_counts_window ON repository_traffic_counts(repo_id,kind,day);
CREATE INDEX idx_repository_traffic_counts_paths ON repository_traffic_counts(repo_id,kind,path_hash,last_seen DESC);
CREATE INDEX idx_repository_traffic_counts_retention ON repository_traffic_counts(day);
CREATE INDEX idx_repository_traffic_visitors_retention ON repository_traffic_visitors(day);
CREATE INDEX idx_repository_traffic_keys_retention ON repository_traffic_keys(day);

CREATE TABLE repository_immutable_releases (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE
);

-- Publication and namespace history outlive deletion, so an old immutable tag cannot be reused.
CREATE TABLE immutable_release_publications (
    repo_id INTEGER NOT NULL,
    tag_name TEXT NOT NULL CHECK(length(tag_name) BETWEEN 1 AND 255),
    tag_oid TEXT NOT NULL CHECK(length(tag_oid)=40),
    commit_sha TEXT NOT NULL CHECK(length(commit_sha)=40),
    release_id INTEGER UNIQUE REFERENCES releases(id) ON DELETE SET NULL,
    signer_key TEXT NOT NULL CHECK(length(signer_key)=64),
    attestation TEXT NOT NULL CHECK(length(attestation)<=2097152),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    PRIMARY KEY(repo_id,tag_name)
);

CREATE TABLE immutable_release_repository_names (
    repo_id INTEGER NOT NULL,
    owner_kind TEXT NOT NULL CHECK(owner_kind IN ('user','org')),
    owner_id INTEGER NOT NULL,
    name_key TEXT NOT NULL,
    PRIMARY KEY(repo_id,owner_kind,owner_id,name_key)
);
CREATE INDEX idx_immutable_release_repository_names_owner ON immutable_release_repository_names(owner_kind,owner_id,name_key);

CREATE TABLE immutable_release_owner_names (
    owner_kind TEXT NOT NULL CHECK(owner_kind IN ('user','org')),
    owner_id INTEGER NOT NULL,
    name_key TEXT NOT NULL,
    PRIMARY KEY(owner_kind,owner_id,name_key)
);
CREATE INDEX idx_immutable_release_owner_names_name ON immutable_release_owner_names(name_key);
CREATE TRIGGER immutable_release_capture_repository AFTER UPDATE OF name,owner_kind,owner_user_id,owner_org_id ON repositories
WHEN EXISTS(SELECT 1 FROM immutable_release_publications WHERE repo_id=NEW.id)
BEGIN
    INSERT INTO immutable_release_repository_names(repo_id,owner_kind,owner_id,name_key)
    VALUES(NEW.id,NEW.owner_kind,COALESCE(NEW.owner_user_id,NEW.owner_org_id),LOWER(NEW.name)) ON CONFLICT DO NOTHING;
    INSERT INTO immutable_release_owner_names(owner_kind,owner_id,name_key)
    SELECT NEW.owner_kind,COALESCE(NEW.owner_user_id,NEW.owner_org_id),LOWER(COALESCE(u.username,g.name))
    FROM repositories r LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations g ON g.id=r.owner_org_id WHERE r.id=NEW.id ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER immutable_release_capture_user AFTER UPDATE OF username ON users
WHEN EXISTS(SELECT 1 FROM immutable_release_owner_names WHERE owner_kind='user' AND owner_id=NEW.id)
BEGIN
    INSERT INTO immutable_release_owner_names(owner_kind,owner_id,name_key) VALUES('user',NEW.id,LOWER(NEW.username)) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER immutable_release_capture_organization AFTER UPDATE OF name ON organizations
WHEN EXISTS(SELECT 1 FROM immutable_release_owner_names WHERE owner_kind='org' AND owner_id=NEW.id)
BEGIN
    INSERT INTO immutable_release_owner_names(owner_kind,owner_id,name_key) VALUES('org',NEW.id,LOWER(NEW.name)) ON CONFLICT DO NOTHING;
END;

-- An owner rename must not resurrect tags in a retired namespace. Move conflicting
-- repositories first; the primary can inspect their actual tags when renaming/transferring.
CREATE TRIGGER immutable_release_guard_user BEFORE UPDATE OF username ON users
WHEN LOWER(OLD.username)<>LOWER(NEW.username) AND EXISTS(SELECT 1 FROM repositories c JOIN immutable_release_repository_names n ON n.name_key=LOWER(c.name) AND n.repo_id<>c.id JOIN immutable_release_owner_names o ON o.owner_kind=n.owner_kind AND o.owner_id=n.owner_id WHERE c.owner_user_id=NEW.id AND o.name_key=LOWER(NEW.username) AND NOT(o.owner_kind='user' AND o.owner_id=NEW.id))
BEGIN SELECT RAISE(ABORT,'Destination owner name has reserved immutable repository history; move conflicting repositories first'); END;
CREATE TRIGGER immutable_release_guard_organization BEFORE UPDATE OF name ON organizations
WHEN LOWER(OLD.name)<>LOWER(NEW.name) AND EXISTS(SELECT 1 FROM repositories c JOIN immutable_release_repository_names n ON n.name_key=LOWER(c.name) AND n.repo_id<>c.id JOIN immutable_release_owner_names o ON o.owner_kind=n.owner_kind AND o.owner_id=n.owner_id WHERE c.owner_org_id=NEW.id AND o.name_key=LOWER(NEW.name) AND NOT(o.owner_kind='org' AND o.owner_id=NEW.id))
BEGIN SELECT RAISE(ABORT,'Destination owner name has reserved immutable repository history; move conflicting repositories first'); END;

-- repos dependency security
CREATE TABLE repository_dependency_security (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    alerts_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    updates_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    alerts_actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    updates_actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    owner_kind TEXT NOT NULL CHECK(owner_kind IN ('user','org')),
    owner_id INTEGER NOT NULL CHECK(owner_id>0),
    private_consent BOOLEAN NOT NULL DEFAULT FALSE,
    generation INTEGER NOT NULL DEFAULT 1 CHECK(generation>0),
    next_scan_at TEXT,
    lease_nonce TEXT CHECK(lease_nonce IS NULL OR length(lease_nonce)=36),
    lease_until TEXT,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 20),
    last_status TEXT NOT NULL DEFAULT 'idle' CHECK(last_status IN ('idle','queued','scanning','complete','partial','unsupported','error','paused')),
    last_error TEXT CHECK(last_error IS NULL OR length(last_error)<=1024),
    last_scan_sha TEXT CHECK(last_scan_sha IS NULL OR length(last_scan_sha)=40),
    last_scan_at TEXT,
    last_attempt_at TEXT,
    snapshot TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(snapshot) AND json_type(snapshot)='object' AND length(CAST(snapshot AS BLOB))<=4194304),
    CHECK(NOT updates_enabled OR alerts_enabled),
    CHECK((lease_nonce IS NULL)=(lease_until IS NULL))
);
CREATE INDEX idx_dependency_security_due ON repository_dependency_security(next_scan_at,repo_id) WHERE alerts_enabled;
CREATE TRIGGER dependency_security_reauthorize AFTER UPDATE OF owner_kind,owner_user_id,owner_org_id,is_private ON repositories
WHEN OLD.owner_kind IS NOT NEW.owner_kind OR OLD.owner_user_id IS NOT NEW.owner_user_id OR OLD.owner_org_id IS NOT NEW.owner_org_id OR (NOT OLD.is_private AND NEW.is_private)
BEGIN
    UPDATE repository_dependency_security SET alerts_actor_id=NULL,updates_enabled=FALSE,updates_actor_id=NULL,private_consent=FALSE,generation=generation+1,lease_nonce=NULL,lease_until=NULL,next_scan_at=NULL,last_status='paused',last_error='Repository ownership or privacy changed; a current administrator must reauthorize dependency security.' WHERE repo_id=NEW.id AND alerts_enabled;
END;
CREATE TRIGGER dependency_security_default AFTER UPDATE OF default_branch ON repositories WHEN OLD.default_branch IS NOT NEW.default_branch
BEGIN
    UPDATE repository_dependency_security SET generation=generation+1,next_scan_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),last_status=CASE WHEN lease_nonce IS NULL THEN 'queued' ELSE last_status END WHERE repo_id=NEW.id AND alerts_enabled AND alerts_actor_id IS NOT NULL;
END;

-- Dependabot repository access
CREATE TABLE org_dependabot_access (
    org_id INTEGER PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    default_level TEXT NOT NULL CHECK(default_level IN ('public','internal'))
);

CREATE TABLE org_dependabot_access_repositories (
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(org_id,repo_id)
);
CREATE INDEX org_dependabot_access_repositories_repo ON org_dependabot_access_repositories(repo_id);

-- Private registries
CREATE TABLE org_private_registries (
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    configuration TEXT NOT NULL,
    encrypted_value BLOB,
    nonce BLOB CHECK(nonce IS NULL OR length(nonce)=12),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK((encrypted_value IS NULL) = (nonce IS NULL)),
    PRIMARY KEY(org_id,name)
);

-- Dependabot alerts
CREATE TABLE global_advisories (
    ghsa_id TEXT PRIMARY KEY,
    document TEXT NOT NULL,
    osv_id TEXT REFERENCES osv_advisories(id)
);

CREATE TABLE dependabot_alerts (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    ghsa_id TEXT NOT NULL,
    manifest TEXT NOT NULL,
    manifest_hash TEXT NOT NULL CHECK(length(manifest_hash)=64),
    ecosystem TEXT NOT NULL,
    package TEXT NOT NULL,
    version TEXT NOT NULL,
    scope TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('open','dismissed','fixed')),
    advisory TEXT NOT NULL,
    vulnerability TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    fixed_at TEXT,
    dismissed_at TEXT,
    dismissed_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    dismissed_reason TEXT,
    dismissed_comment TEXT,
    PRIMARY KEY(repo_id,number),
    UNIQUE(repo_id,ghsa_id,manifest_hash,ecosystem,package)
);

-- Actions concurrency groups
CREATE TABLE actions_concurrency_groups (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name_key TEXT NOT NULL,
    name TEXT NOT NULL,
    last_acquired_at TEXT,
    next_request BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY(repo_id, name_key)
);

CREATE TABLE actions_run_concurrency (
    run_id INTEGER PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL,
    name_key TEXT NOT NULL,
    holding BOOLEAN NOT NULL DEFAULT FALSE,
    request_order BIGINT NOT NULL DEFAULT 0,
    cancel_in_progress BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY(repo_id, name_key) REFERENCES actions_concurrency_groups(repo_id, name_key) ON DELETE CASCADE
);
CREATE INDEX actions_run_concurrency_group ON actions_run_concurrency(repo_id, name_key);

-- Organization rulesets
CREATE TABLE repository_rulesets (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK(typeof(id)='integer'),
    owner_org_id INTEGER REFERENCES organizations(id) ON DELETE CASCADE CHECK(owner_org_id IS NULL OR typeof(owner_org_id)='integer'),
    owner_repo_id INTEGER REFERENCES repositories(id) ON DELETE CASCADE CHECK(owner_repo_id IS NULL OR typeof(owner_repo_id)='integer'),
    definition TEXT NOT NULL CHECK(typeof(definition)='text' AND length(CAST(definition AS BLOB))<=262144),
    created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    CHECK((owner_org_id IS NOT NULL AND owner_repo_id IS NULL) OR (owner_org_id IS NULL AND owner_repo_id IS NOT NULL))
);
CREATE INDEX repository_rulesets_org ON repository_rulesets(owner_org_id,id);
CREATE INDEX repository_rulesets_repo ON repository_rulesets(owner_repo_id,id);

CREATE TABLE repository_ruleset_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK(typeof(id)='integer'),
    ruleset_id INTEGER NOT NULL REFERENCES repository_rulesets(id) ON DELETE CASCADE CHECK(typeof(ruleset_id)='integer'),
    definition TEXT NOT NULL CHECK(typeof(definition)='text' AND length(CAST(definition AS BLOB))<=262144),
    -- Historical principal metadata, not a live authority grant; deletion does not rewrite it.
    actor_id INTEGER CHECK(actor_id IS NULL OR (typeof(actor_id)='integer' AND actor_id>0)),
    actor_type TEXT NOT NULL CHECK(actor_type IN ('User','Integration','DeployKey','System')),
    created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX repository_ruleset_versions_rule ON repository_ruleset_versions(ruleset_id,id DESC);

CREATE TABLE repository_rule_suites (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK(typeof(id)='integer'),
    org_id INTEGER REFERENCES organizations(id) ON DELETE SET NULL CHECK(org_id IS NULL OR typeof(org_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK(typeof(repo_id)='integer'),
    repository_name TEXT NOT NULL CHECK(length(repository_name)<=255),
    actor_id INTEGER CHECK(actor_id IS NULL OR (typeof(actor_id)='integer' AND actor_id>0)),
    actor_name TEXT CHECK(length(actor_name)<=255),
    ref_name TEXT NOT NULL CHECK(length(ref_name)<=1024),
    before_sha TEXT NOT NULL CHECK(length(before_sha) IN (40,64)),
    after_sha TEXT NOT NULL CHECK(length(after_sha) IN (40,64)),
    result TEXT NOT NULL CHECK(result IN ('pass','fail','bypass')),
    evaluation_result TEXT CHECK(evaluation_result IS NULL OR evaluation_result IN ('pass','fail','bypass')),
    evaluations TEXT NOT NULL CHECK(typeof(evaluations)='text' AND length(CAST(evaluations AS BLOB))<=16777216),
    created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);
CREATE INDEX repository_rule_suites_org_time ON repository_rule_suites(org_id, created_at, id);
CREATE INDEX repository_rule_suites_repo_time ON repository_rule_suites(repo_id, created_at, id);
CREATE INDEX repository_rule_suites_org ON repository_rule_suites(org_id,id DESC);
CREATE INDEX repository_rule_suites_repo ON repository_rule_suites(repo_id,id DESC);

-- deployments: custom App protection rules and attempt-scoped decisions
CREATE TABLE deployment_protection_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    environment_id BIGINT NOT NULL REFERENCES deployment_environments(id) ON DELETE CASCADE,
    app_id BIGINT NOT NULL,
    slug TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    UNIQUE(environment_id,app_id)
);

-- App removal must not turn an outstanding approval into permission to deploy.
CREATE TABLE deployment_protection_gates (
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    rule_id BIGINT NOT NULL,
    app_id BIGINT NOT NULL,
    environment_name TEXT NOT NULL,
    environment_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','approved','rejected','disabled')),
    comment TEXT NOT NULL DEFAULT '',
    PRIMARY KEY(run_id,rule_id),
    UNIQUE(run_id,app_id,environment_key)
);
CREATE INDEX deployment_protection_gates_rule ON deployment_protection_gates(rule_id);

-- Actions OIDC
CREATE TABLE actions_oidc_key (
    id INTEGER PRIMARY KEY CHECK(id=1),
    private_key TEXT NOT NULL
);

CREATE TABLE actions_oidc_jobs (
    token_id BIGINT PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    workflow TEXT NOT NULL,
    workflow_name TEXT NOT NULL,
    environment TEXT
);

CREATE TABLE actions_oidc_repo_settings (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    use_default BOOLEAN NOT NULL,
    claim_keys TEXT
);

CREATE TABLE actions_oidc_org_settings (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    use_default BOOLEAN NOT NULL DEFAULT false,
    claim_keys TEXT
);

CREATE TABLE actions_oidc_properties (
    definition_id BIGINT PRIMARY KEY REFERENCES organization_repository_properties(id) ON DELETE CASCADE
);

-- Issue management
CREATE TABLE pinned_issues (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    issue_id INTEGER NOT NULL UNIQUE REFERENCES issues(id) ON DELETE CASCADE,
    slot INTEGER NOT NULL CHECK(slot BETWEEN 1 AND 3),
    PRIMARY KEY(repo_id, slot)
);

CREATE TABLE issue_redirects (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    PRIMARY KEY(repo_id, number)
);

CREATE TABLE issue_duplicates (
    issue_id INTEGER PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE,
    duplicate_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    CHECK(issue_id <> duplicate_id)
);

CREATE TABLE issue_linked_branches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    UNIQUE(issue_id, repo_id, name)
);

CREATE TABLE ci_schedule_runs (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_path TEXT NOT NULL,
    cron TEXT NOT NULL,
    minute BIGINT NOT NULL,
    -- Deleting the run keeps the minute, so another node does not queue it again.
    run_id BIGINT REFERENCES ci_runs(id) ON DELETE SET NULL,
    PRIMARY KEY (repo_id, workflow_path, cron)
);

-- Branch renames.
CREATE TABLE branch_redirects (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    old_name TEXT NOT NULL,
    new_name TEXT NOT NULL,
    PRIMARY KEY(repo_id, old_name)
);

-- Pull request auto-merge: the background check merges once the requirements pass.
CREATE TABLE pull_request_auto_merges (
    pull_request_id INTEGER PRIMARY KEY REFERENCES pull_requests(id) ON DELETE CASCADE,
    enabled_by INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_id INTEGER REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    merge_method TEXT NOT NULL CHECK(merge_method IN ('merge','squash','rebase')),
    commit_title TEXT,
    commit_message TEXT,
    author_email TEXT,
    generation TEXT NOT NULL DEFAULT(lower(hex(randomblob(16)))),
    enabled_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

-- Job-level Actions concurrency. Groups share their names with workflow-level groups.
CREATE TABLE actions_job_concurrency (
    job_id INTEGER PRIMARY KEY REFERENCES actions_jobs(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL,
    name_key TEXT NOT NULL,
    holding BOOLEAN NOT NULL DEFAULT FALSE,
    request_order BIGINT NOT NULL DEFAULT 0,
    cancelled BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY(repo_id, name_key) REFERENCES actions_concurrency_groups(repo_id, name_key) ON DELETE CASCADE
);
CREATE INDEX actions_job_concurrency_group ON actions_job_concurrency(repo_id, name_key);

-- Reverse references and ordered collection lookups.
CREATE INDEX actions_blob_grants_object ON actions_blob_grants(object_id);
CREATE INDEX actions_blob_grants_run ON actions_blob_grants(run_id);
CREATE INDEX actions_artifacts_repository ON actions_objects(repo_id,kind,finalized,created_at,id);
CREATE INDEX package_blob_grants_digest ON package_blob_grants(digest);
CREATE INDEX package_blob_grants_expiry ON package_blob_grants(expires_at);
CREATE INDEX global_advisories_osv ON global_advisories(osv_id);
CREATE INDEX project_items_issue ON project_items(issue_id,project_id);
CREATE INDEX project_items_pull ON project_items(pull_request_id,project_id);
CREATE INDEX project_items_repository ON project_items(repo_id);
CREATE INDEX project_repositories_repository ON project_repositories(repo_id,project_id);
CREATE INDEX discussion_comments_reply ON discussion_comments(reply_to_id);
CREATE INDEX discussions_answer ON discussions(answer_id);
CREATE INDEX issue_field_values_attribute ON issue_field_values(attribute_id);
CREATE INDEX issue_type_assignments_attribute ON issue_type_assignments(attribute_id);
CREATE INDEX ci_accepted_pushes_history ON ci_accepted_pushes(repo_id,reference,sha,accepted_at DESC);
CREATE INDEX stars_roster ON stars(repo_id,created_at DESC,user_id DESC);
CREATE INDEX watches_roster ON watches(repo_id,created_at DESC,user_id DESC) WHERE level IN ('watching','custom');
CREATE INDEX pull_requests_merged_commit ON pull_requests(repo_id,merge_commit_sha) WHERE merged;
CREATE INDEX issue_comments_author_activity ON issue_comments(author_id,created_at DESC,id DESC);
CREATE INDEX pr_comments_author_activity ON pr_comments(author_id,created_at DESC,id DESC);
CREATE INDEX packages_deleted ON packages(deleted_at) WHERE deleted_at IS NOT NULL;
CREATE INDEX package_versions_deleted ON package_versions(deleted_at) WHERE deleted_at IS NOT NULL;
CREATE INDEX package_blobs_expiry ON package_blobs(expires_at);
CREATE INDEX package_blobs_deleting ON package_blobs(digest) WHERE deleting;
CREATE INDEX package_registry_tokens_expiry ON package_registry_tokens(expires_at);
CREATE INDEX package_uploads_expiry ON package_uploads(expires_at);

-- Bumped by every write that can change a merge decision. A merge compares it with the revision
-- it was judged under right before the branch moves.
CREATE TABLE repository_merge_epochs (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    version INTEGER NOT NULL DEFAULT 0 CHECK(version>=0)
);
CREATE TRIGGER merge_policy_repository AFTER UPDATE OF is_archived,is_private,allow_merge_commit,allow_squash_merge,allow_rebase_merge,owner_kind,owner_user_id,owner_org_id,storage_node_id,default_branch ON repositories BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=NEW.id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_rules_insert AFTER INSERT ON branch_protection_rules BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=NEW.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_rules_update AFTER UPDATE ON branch_protection_rules BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (OLD.repo_id,NEW.repo_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_rules_delete AFTER DELETE ON branch_protection_rules BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=OLD.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
-- Text, counts, milestones, mergeability and lock state never decide a merge, so editing them
-- does not bump the epoch. A new pull request, review or comment column joins these lists unless
-- it is just as irrelevant; tests/database/native.rs checks every column.
CREATE TRIGGER merge_policy_pulls_insert AFTER INSERT ON pull_requests BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=NEW.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_pulls_update AFTER UPDATE OF id,repo_id,number,author_id,state,head_repo_id,workflows_approved_sha,head_branch,head_sha,base_branch,base_sha,fork_sha,is_draft,merged,merged_by_id,closed_by_id,merge_commit_sha,stack_id,stack_version,created_at,merged_at,closed_at,maintainer_can_modify,maintainer_edit_granted_by_id,source_issue_id,head_publisher_id ON pull_requests BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (OLD.repo_id,NEW.repo_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_pulls_delete AFTER DELETE ON pull_requests BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=OLD.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_reviews_insert AFTER INSERT ON pr_reviews BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=(SELECT repo_id FROM pull_requests WHERE id=NEW.pull_request_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_reviews_update AFTER UPDATE OF id,pull_request_id,author_id,state,commit_sha,dismissed_by_id,dismissed_reason,dismissed_at,created_at ON pr_reviews BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (SELECT repo_id FROM pull_requests WHERE id IN (OLD.pull_request_id,NEW.pull_request_id)) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_reviews_delete AFTER DELETE ON pr_reviews BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=(SELECT repo_id FROM pull_requests WHERE id=OLD.pull_request_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_threads_insert AFTER INSERT ON pr_comments BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=(SELECT repo_id FROM pull_requests WHERE id=NEW.pull_request_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_threads_update AFTER UPDATE OF id,pull_request_id,author_id,path,line,side,commit_sha,diff_base_sha,in_reply_to_comment_id,resolved_at,resolved_by_id,review_id,created_at,start_line,start_side,source_issue_comment_id ON pr_comments BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (SELECT repo_id FROM pull_requests WHERE id IN (OLD.pull_request_id,NEW.pull_request_id)) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_threads_delete AFTER DELETE ON pr_comments BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=(SELECT repo_id FROM pull_requests WHERE id=OLD.pull_request_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_statuses_insert AFTER INSERT ON commit_statuses BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=NEW.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_statuses_update AFTER UPDATE ON commit_statuses BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (OLD.repo_id,NEW.repo_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_statuses_delete AFTER DELETE ON commit_statuses BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=OLD.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_checks_insert AFTER INSERT ON check_runs BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=NEW.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_checks_update AFTER UPDATE ON check_runs BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (OLD.repo_id,NEW.repo_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_checks_delete AFTER DELETE ON check_runs BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=OLD.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_suites_insert AFTER INSERT ON check_suites BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=NEW.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_suites_update AFTER UPDATE ON check_suites BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (OLD.repo_id,NEW.repo_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_suites_delete AFTER DELETE ON check_suites BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=OLD.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_ci_insert AFTER INSERT ON ci_runs BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=NEW.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_ci_update AFTER UPDATE OF repo_id,head_sha,workflow_path,status,attempt,created_at ON ci_runs BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id IN (OLD.repo_id,NEW.repo_id) ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
CREATE TRIGGER merge_policy_ci_delete AFTER DELETE ON ci_runs BEGIN
    INSERT INTO repository_merge_epochs SELECT id,1 FROM repositories WHERE id=OLD.repo_id ON CONFLICT(repo_id) DO UPDATE SET version=version+1;
END;
