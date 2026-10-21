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
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

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
    id TEXT NOT NULL PRIMARY KEY CHECK (length(id)<=64),
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
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
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

CREATE TABLE gpg_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    key_id TEXT NOT NULL UNIQUE CHECK (length(key_id)<=40),
    public_key TEXT NOT NULL,
    emails TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(emails) AND json_type(emails)='array'),
    can_sign INTEGER NOT NULL DEFAULT 1 CHECK (can_sign IN (0,1)),
    can_encrypt INTEGER NOT NULL DEFAULT 0 CHECK (can_encrypt IN (0,1)),
    expires_at TEXT,
    verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0,1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
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
    UNIQUE(org_id, user_id)
);

-- Organization invitations: a pending offer of membership. Membership starts only when the
-- invitee accepts; a decline or a cancellation deletes the row.
CREATE TABLE organization_invitations (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK (typeof(org_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    role TEXT NOT NULL DEFAULT 'member' CHECK (length(role)<=20),
    invited_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (invited_by_id IS NULL OR typeof(invited_by_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
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

CREATE TABLE repo_language_cache (
    repo_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    storage_node_id INTEGER,
    write_version INTEGER NOT NULL,
    revision TEXT NOT NULL
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
CREATE INDEX idx_timeline_events_repo_id ON timeline_events(repo_id);

CREATE TABLE reactions (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('issue','issue_comment','pull_request','pr_comment','pr_review')),
    subject_id INTEGER NOT NULL CHECK (typeof(subject_id)='integer'),
    content TEXT NOT NULL CHECK (content IN ('+1','-1','laugh','hooray','confused','heart','rocket','eyes')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(user_id, subject_kind, subject_id, content)
);
CREATE INDEX idx_reactions_subject ON reactions(subject_kind, subject_id);
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
    merge_commit_sha TEXT CHECK (length(merge_commit_sha)<=40),
    stack_id INTEGER CHECK (stack_id IS NULL OR typeof(stack_id)='integer'),
    milestone_id INTEGER REFERENCES milestones(id) ON DELETE SET NULL CHECK (milestone_id IS NULL OR typeof(milestone_id)='integer'),
    additions INTEGER NOT NULL DEFAULT 0 CHECK (additions IS NULL OR typeof(additions)='integer') CHECK (additions BETWEEN -2147483648 AND 2147483647),
    deletions INTEGER NOT NULL DEFAULT 0 CHECK (deletions IS NULL OR typeof(deletions)='integer') CHECK (deletions BETWEEN -2147483648 AND 2147483647),
    changed_files INTEGER NOT NULL DEFAULT 0 CHECK (changed_files IS NULL OR typeof(changed_files)='integer') CHECK (changed_files BETWEEN -2147483648 AND 2147483647),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    merged_at TEXT,
    closed_at TEXT,
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
    in_reply_to_comment_id INTEGER REFERENCES pr_comments(id) ON DELETE CASCADE CHECK (in_reply_to_comment_id IS NULL OR typeof(in_reply_to_comment_id)='integer'),
    resolved_at TEXT,
    resolved_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (resolved_by_id IS NULL OR typeof(resolved_by_id)='integer'),
    -- The review this comment was filed in. A pending review's comments stay private to its author.
    review_id INTEGER REFERENCES pr_reviews(id) ON DELETE CASCADE CHECK (review_id IS NULL OR typeof(review_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
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
    UNIQUE(repo_id, tag_name)
);

-- One latest release per repository.
CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_one_latest ON releases(repo_id) WHERE is_latest = 1;

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

CREATE UNIQUE INDEX IF NOT EXISTS idx_release_assets_name ON release_assets(release_id, name);
CREATE INDEX IF NOT EXISTS idx_release_assets_file_id ON release_assets(file_id);

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

CREATE TABLE pinned_repos (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (user_id IS NULL OR typeof(user_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    position INTEGER NOT NULL DEFAULT 0 CHECK (position IS NULL OR typeof(position)='integer') CHECK (position BETWEEN -32768 AND 32767),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(user_id, repo_id)
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

CREATE TABLE org_secrets (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    org_id INTEGER NOT NULL REFERENCES organizations(id) ON DELETE CASCADE CHECK (org_id IS NULL OR typeof(org_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=255),
    encrypted_value BLOB NOT NULL,
    nonce BLOB NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'all' CHECK (length(visibility)<=20), 
    created_by_id INTEGER REFERENCES users(id) ON DELETE SET NULL CHECK (created_by_id IS NULL OR typeof(created_by_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(org_id, name)
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
    uploaded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

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
    scope_repository_id INTEGER NOT NULL CHECK (scope_repository_id >= 0)
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
    branch TEXT NOT NULL CHECK (length(branch)<=255),
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
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_ci_runs_pull_revision ON ci_runs(pull_request_id, branch, head_sha, workflow_path, attempt)
    WHERE event = 'pull_request' AND pull_request_id IS NOT NULL;

CREATE TABLE ci_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE CHECK (run_id IS NULL OR typeof(run_id)='integer'),
    stage TEXT NOT NULL CHECK (length(stage)<=255),
    line TEXT NOT NULL,
    level TEXT NOT NULL DEFAULT 'info' CHECK (length(level)<=20), 
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE ci_artifacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE CHECK (run_id IS NULL OR typeof(run_id)='integer'),
    name TEXT NOT NULL CHECK (length(name)<=255),
    storage_url TEXT NOT NULL,
    size_bytes INTEGER CHECK (size_bytes IS NULL OR typeof(size_bytes)='integer'),
    content_type TEXT CHECK (length(content_type)<=100),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE ci_deployments (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    run_id INTEGER NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE CHECK (run_id IS NULL OR typeof(run_id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (repo_id IS NULL OR typeof(repo_id)='integer'),
    environment TEXT NOT NULL CHECK (length(environment)<=255),
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
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE join_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash)=64 AND token_hash NOT GLOB '*[^0-9a-f]*'),
    roles TEXT NOT NULL CHECK (json_valid(roles) AND json_type(roles)='array' AND json_array_length(roles)>0),
    reusable INTEGER NOT NULL DEFAULT 0 CHECK (reusable IN (0,1)),
    requires_approval INTEGER NOT NULL DEFAULT 0 CHECK (requires_approval IN (0,1)),
    expires_at TEXT NOT NULL,
    uses INTEGER NOT NULL DEFAULT 0 CHECK (typeof(uses)='integer' AND uses>=0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
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

CREATE INDEX IF NOT EXISTS idx_repository_replicas_node ON repository_replicas(node_id);

CREATE INDEX IF NOT EXISTS idx_files_user_id ON files(user_id);


CREATE INDEX IF NOT EXISTS idx_files_is_active ON files(is_active);

CREATE INDEX IF NOT EXISTS idx_files_created_at ON files(created_at);

CREATE INDEX IF NOT EXISTS idx_files_reason ON files(reason);

CREATE INDEX IF NOT EXISTS idx_users_avatar_file ON users(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_signup_requests_username_pending
ON signup_requests (username_key)
WHERE status = 'pending';

CREATE UNIQUE INDEX IF NOT EXISTS idx_signup_requests_email_pending
ON signup_requests (email_key)
WHERE email IS NOT NULL AND status = 'pending';

CREATE INDEX IF NOT EXISTS idx_signup_requests_status_created_at
ON signup_requests (status, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_signup_requests_processed_hash ON signup_requests(id) WHERE status <> 'pending' AND password_hash <> '';

CREATE INDEX IF NOT EXISTS idx_oauth_accounts_user_id ON oauth_accounts(user_id);

CREATE INDEX IF NOT EXISTS idx_sessions_user_id ON sessions(user_id);

CREATE INDEX IF NOT EXISTS idx_sessions_expires_at ON sessions(expires_at);

CREATE INDEX IF NOT EXISTS idx_anonymous_sessions_expires_at ON anonymous_sessions(expires_at);

CREATE INDEX IF NOT EXISTS idx_oauth_states_expires_at ON oauth_states(expires_at);

CREATE INDEX IF NOT EXISTS idx_ssh_keys_user_id ON ssh_keys(user_id);

CREATE INDEX IF NOT EXISTS idx_gpg_keys_user_id ON gpg_keys(user_id);

CREATE INDEX IF NOT EXISTS idx_organizations_avatar_file ON organizations(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_org_memberships_user_id ON org_memberships(user_id);

CREATE INDEX IF NOT EXISTS idx_team_memberships_user_id ON team_memberships(user_id);

CREATE INDEX IF NOT EXISTS idx_repositories_storage_node ON repositories(storage_node_id, id);

CREATE INDEX IF NOT EXISTS idx_repositories_name ON repositories(name);
CREATE INDEX IF NOT EXISTS idx_repositories_storage_uuid ON repositories (substr(disk_path, -40));

CREATE INDEX IF NOT EXISTS idx_repositories_is_private ON repositories(is_private);

CREATE INDEX IF NOT EXISTS idx_repositories_forked_from_id ON repositories(forked_from_id);

CREATE INDEX IF NOT EXISTS idx_repositories_created_at ON repositories(created_at);

CREATE INDEX IF NOT EXISTS idx_repositories_updated_at ON repositories(updated_at);
CREATE INDEX IF NOT EXISTS idx_repositories_created_by ON repositories(created_by_id);
CREATE INDEX IF NOT EXISTS idx_repositories_public_stars
    ON repositories(stars_count DESC, created_at DESC, id DESC) WHERE is_private = false;
CREATE INDEX IF NOT EXISTS idx_repositories_search_order
    ON repositories(stars_count DESC, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_repo_access_repo_id ON repo_access(repo_id);

CREATE INDEX IF NOT EXISTS idx_team_repo_access_repo_id ON team_repo_access(repo_id);

CREATE INDEX IF NOT EXISTS idx_deploy_keys_fingerprint ON deploy_keys(fingerprint);

CREATE INDEX IF NOT EXISTS idx_repo_storage_history_repo_time ON repo_storage_history(repo_id, recorded_at DESC);

CREATE INDEX IF NOT EXISTS idx_repo_storage_history_recorded_at ON repo_storage_history(recorded_at);

CREATE INDEX IF NOT EXISTS idx_commit_statuses_ci_run_id ON commit_statuses(ci_run_id) WHERE ci_run_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_issues_author_id ON issues(author_id);
CREATE INDEX IF NOT EXISTS idx_issue_assignees_user ON issue_assignees(user_id);

CREATE INDEX IF NOT EXISTS idx_issues_state ON issues(state);

CREATE INDEX IF NOT EXISTS idx_issues_repo_state ON issues(repo_id, state);

CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_issues_milestone_id ON issues(milestone_id);

CREATE INDEX IF NOT EXISTS idx_issue_comments_issue_id ON issue_comments(issue_id);

CREATE INDEX IF NOT EXISTS idx_issue_comments_author_id ON issue_comments(author_id);

CREATE INDEX IF NOT EXISTS idx_issue_comments_created_at ON issue_comments(created_at);

CREATE INDEX IF NOT EXISTS idx_pull_requests_author_id ON pull_requests(author_id);

CREATE INDEX IF NOT EXISTS idx_pull_requests_state ON pull_requests(state);

CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_state ON pull_requests(repo_id, state);
CREATE INDEX IF NOT EXISTS idx_pull_requests_head_repo_state ON pull_requests(head_repo_id, state);

CREATE INDEX IF NOT EXISTS idx_pull_requests_created_at ON pull_requests(created_at);
CREATE INDEX IF NOT EXISTS idx_pull_requests_updated_at ON pull_requests(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_stack ON pull_requests(repo_id, stack_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_milestone_id ON pull_requests(milestone_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_merged_by ON pull_requests(merged_by_id, merged_at) WHERE merged_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_issue_labels_label ON issue_labels(label_id);
CREATE INDEX IF NOT EXISTS idx_pr_labels_label ON pr_labels(label_id);
CREATE INDEX IF NOT EXISTS idx_pr_assignees_user ON pr_assignees(user_id);
CREATE INDEX IF NOT EXISTS idx_pr_reviewers_user ON pr_reviewers(user_id);

CREATE INDEX IF NOT EXISTS idx_pr_comments_pr_id ON pr_comments(pull_request_id);

CREATE INDEX IF NOT EXISTS idx_pr_comments_author_id ON pr_comments(author_id);

CREATE INDEX IF NOT EXISTS idx_pr_comments_created_at ON pr_comments(created_at);

CREATE INDEX IF NOT EXISTS idx_pr_reviews_pr_id ON pr_reviews(pull_request_id);

CREATE INDEX IF NOT EXISTS idx_pr_reviews_author_id ON pr_reviews(author_id);

CREATE INDEX IF NOT EXISTS idx_pr_reviews_created_at ON pr_reviews(created_at);

-- Each author has at most one pending review on a pull request.
CREATE UNIQUE INDEX IF NOT EXISTS idx_pr_reviews_one_pending ON pr_reviews(pull_request_id, author_id) WHERE state = 'pending';

CREATE INDEX IF NOT EXISTS idx_pr_comments_reply_to ON pr_comments(in_reply_to_comment_id);

CREATE INDEX IF NOT EXISTS idx_pr_comments_review_id ON pr_comments(review_id);

CREATE INDEX IF NOT EXISTS idx_releases_author_id ON releases(author_id);

CREATE INDEX IF NOT EXISTS idx_releases_created_at ON releases(created_at);

CREATE INDEX IF NOT EXISTS idx_stars_repo_id ON stars(repo_id);

CREATE INDEX IF NOT EXISTS idx_watches_repo_id ON watches(repo_id);

CREATE INDEX IF NOT EXISTS idx_follows_following_id ON follows(following_id);

CREATE INDEX IF NOT EXISTS idx_pinned_repos_user_id ON pinned_repos(user_id);

CREATE INDEX IF NOT EXISTS idx_audit_log_org_id ON audit_log(org_id);

CREATE INDEX IF NOT EXISTS idx_audit_log_actor_id ON audit_log(actor_id);

CREATE INDEX IF NOT EXISTS idx_audit_log_action ON audit_log(action);

CREATE INDEX IF NOT EXISTS idx_audit_log_created_at ON audit_log(created_at);

CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_user_id ON upload_rate_limit(user_id);

CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_uploaded_at ON upload_rate_limit(uploaded_at);

CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_reason ON upload_rate_limit(reason);

CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_ip ON upload_rate_limit(ip_address);
CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_file_id ON upload_rate_limit(file_id);
CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_user_reason_time ON upload_rate_limit(user_id, reason, uploaded_at);

CREATE INDEX IF NOT EXISTS idx_download_rate_limit_user_id ON download_rate_limit(user_id);

CREATE INDEX IF NOT EXISTS idx_download_rate_limit_downloaded_at ON download_rate_limit(downloaded_at);

CREATE INDEX IF NOT EXISTS idx_download_rate_limit_reason ON download_rate_limit(reason);

CREATE INDEX IF NOT EXISTS idx_download_rate_limit_ip ON download_rate_limit(ip_address);
CREATE INDEX IF NOT EXISTS idx_download_rate_limit_file_id ON download_rate_limit(file_id);

CREATE INDEX IF NOT EXISTS idx_notifications_user_created_at ON notifications(user_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_notifications_user_unread ON notifications(user_id, read_at, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_notifications_user_saved ON notifications(user_id, saved_at, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_notifications_user_done ON notifications(user_id, done_at, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_notifications_actor_id ON notifications(actor_id);
CREATE INDEX IF NOT EXISTS idx_notifications_repo_id ON notifications(repo_id);

CREATE INDEX IF NOT EXISTS idx_ci_runs_repo_created_at ON ci_runs(repo_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_ci_runs_repo_head ON ci_runs(repo_id, head_sha, id);
CREATE INDEX IF NOT EXISTS idx_ci_runs_pull_request ON ci_runs(pull_request_id) WHERE pull_request_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_ci_runs_user_created_at ON ci_runs(triggered_by_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_ci_runs_status ON ci_runs(status);
CREATE INDEX IF NOT EXISTS idx_ci_runs_push_created_at ON ci_runs(created_at) WHERE event = 'push' AND triggered_by_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_ci_runs_push_user_created_at ON ci_runs(triggered_by_id, created_at DESC)
    WHERE event = 'push';

CREATE INDEX IF NOT EXISTS idx_ci_logs_run_id_id ON ci_logs(run_id, id);

CREATE INDEX IF NOT EXISTS idx_ci_artifacts_run_id ON ci_artifacts(run_id);

CREATE INDEX IF NOT EXISTS idx_ci_deployments_repo_env ON ci_deployments(repo_id, environment, created_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_ci_deployments_run ON ci_deployments(run_id);

CREATE INDEX IF NOT EXISTS idx_auth_login_limits_expiry ON auth_login_limits (window_ends_at);

CREATE INDEX IF NOT EXISTS idx_ci_runs_queue ON ci_runs(status, id)
    WHERE status IN ('queued', 'running');
CREATE INDEX IF NOT EXISTS idx_files_pending ON files(upload_expires_at) WHERE NOT is_active;
CREATE INDEX IF NOT EXISTS idx_notifications_scope ON notifications(user_id, scope_repository_id);
CREATE INDEX IF NOT EXISTS idx_repository_pushes_actor ON repository_pushes(actor_id, created_at);
CREATE INDEX IF NOT EXISTS idx_repository_pushes_repo ON repository_pushes(repo_id, created_at);
CREATE INDEX IF NOT EXISTS idx_repository_pushes_created_at ON repository_pushes(created_at);

INSERT INTO file_reason_limits (reason, max_file_size_bytes, max_uploads_per_hour, max_bytes_per_hour, max_uploads_per_day, max_bytes_per_day, allowed_content_types, description) VALUES
('avatar', 2097152, 5, 10485760, 20, 41943040, '["image/jpeg", "image/png", "image/gif", "image/webp"]', 'User profile avatars'),
('org_avatar', 2097152, 5, 10485760, 20, 41943040, '["image/jpeg", "image/png", "image/gif", "image/webp"]', 'Organization avatars'),
('release_asset', 104857600, 20, 524288000, 100, 2147483648, NULL, 'Release binary assets'),
('issue_attachment', 10485760, 20, 104857600, 100, 524288000, '["image/jpeg", "image/png", "image/gif", "image/webp", "application/pdf", "text/plain", "text/markdown"]', 'Issue attachments'),
('pr_attachment', 10485760, 20, 104857600, 100, 524288000, '["image/jpeg", "image/png", "image/gif", "image/webp", "application/pdf", "text/plain", "text/markdown"]', 'PR attachments'),
('readme_image', 5242880, 10, 52428800, 50, 262144000, '["image/jpeg", "image/png", "image/gif", "image/webp", "image/svg+xml"]', 'README and wiki images'),
('repo_asset', 52428800, 10, 262144000, 50, 1073741824, NULL, 'Repository assets'),
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
CREATE TRIGGER watches_insert AFTER INSERT ON watches WHEN NEW.level != 'ignoring' BEGIN
    UPDATE repositories SET watchers_count=watchers_count+1 WHERE id=NEW.repo_id;
END;
CREATE TRIGGER watches_delete AFTER DELETE ON watches WHEN OLD.level != 'ignoring' BEGIN
    UPDATE repositories SET watchers_count=watchers_count-1 WHERE id=OLD.repo_id;
END;
CREATE TRIGGER watches_update AFTER UPDATE OF level,repo_id ON watches BEGIN
    UPDATE repositories SET watchers_count=watchers_count-1 WHERE id=OLD.repo_id AND OLD.level != 'ignoring';
    UPDATE repositories SET watchers_count=watchers_count+1 WHERE id=NEW.repo_id AND NEW.level != 'ignoring';
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
