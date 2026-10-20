-- codeyard PostgreSQL schema.

--------------------------------------------------------------------------------
-- PART 1: FILES (must be first - referenced by users and orgs for avatars)
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS files (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id BIGINT, -- nullable initially, set after users table exists
    filename TEXT NOT NULL,
    original_name TEXT NOT NULL,
    content_type TEXT NOT NULL,
    file_size BIGINT NOT NULL,
    sha256_hex TEXT NOT NULL CHECK (sha256_hex ~ '^[0-9a-f]{64}$'),
    object_key TEXT NOT NULL UNIQUE,
    reason VARCHAR(50),
    visibility TEXT NOT NULL DEFAULT 'owner',
    upload_expires_at TIMESTAMPTZ,
    put_expires_at TIMESTAMPTZ,
    confirmed_at TIMESTAMPTZ,
    is_active BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_files_user_id ON files(user_id);
CREATE INDEX IF NOT EXISTS idx_files_pending ON files(upload_expires_at) WHERE NOT is_active;
CREATE INDEX IF NOT EXISTS idx_files_is_active ON files(is_active);
CREATE INDEX IF NOT EXISTS idx_files_created_at ON files(created_at);
CREATE INDEX IF NOT EXISTS idx_files_reason ON files(reason);

-- Object keys whose stored bytes can be deleted once delete_after has passed.
CREATE TABLE IF NOT EXISTS pending_object_deletions (
    object_key TEXT PRIMARY KEY,
    delete_after TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_pending_object_deletions_due
    ON pending_object_deletions(delete_after);

--------------------------------------------------------------------------------
-- PART 2: USERS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS users (
    id BIGSERIAL PRIMARY KEY,
    username VARCHAR(255) NOT NULL UNIQUE,
    email VARCHAR(255) UNIQUE,
    password_hash VARCHAR(255),
    avatar_file_id UUID REFERENCES files(id) ON DELETE SET NULL,
    display_name VARCHAR(255),
    bio TEXT,
    location VARCHAR(255),
    website VARCHAR(255),
    is_admin BOOLEAN NOT NULL DEFAULT FALSE,
    is_banned BOOLEAN NOT NULL DEFAULT FALSE,
    must_change_password BOOLEAN NOT NULL DEFAULT FALSE,
    email_verified BOOLEAN NOT NULL DEFAULT FALSE,
    two_factor_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);
CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);
CREATE INDEX IF NOT EXISTS idx_users_avatar_file ON users(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

-- files is created before users, so its user foreign key is added here.
ALTER TABLE files ADD CONSTRAINT fk_files_user_id
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE;

--------------------------------------------------------------------------------
-- PART 2B: NODE CONFIGURATION & ACCESS REQUESTS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS node_configuration (
    id SMALLINT PRIMARY KEY,
    node_mode VARCHAR(20) NOT NULL DEFAULT 'public', -- public | private
    setup_completed BOOLEAN NOT NULL DEFAULT FALSE,
    -- '1' keeps one copy (the default), a count such as '2' keeps that many copies in
    -- all, and 'all' keeps a copy on every storage node.
    replicas TEXT NOT NULL DEFAULT '1'
        CHECK (replicas = 'all' OR replicas ~ '^[1-9][0-9]{0,2}$'),
    -- Whether robots.txt and ai.txt block AI training crawlers, and whether HTML pages say so.
    -- Blocked until an administrator allows them.
    ai_crawlers VARCHAR(10) NOT NULL DEFAULT 'block' CHECK (ai_crawlers IN ('allow', 'block')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT node_configuration_singleton CHECK (id = 1),
    CONSTRAINT node_configuration_mode_check CHECK (node_mode IN ('public', 'private'))
);

CREATE TABLE IF NOT EXISTS cluster_ssh_host_key (
    id SMALLINT PRIMARY KEY,
    private_key_pem TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT cluster_ssh_host_key_singleton CHECK (id = 1)
);

CREATE TABLE IF NOT EXISTS signup_requests (
    id BIGSERIAL PRIMARY KEY,
    username VARCHAR(255) NOT NULL,
    username_key TEXT NOT NULL,
    email_key TEXT,
    email VARCHAR(255),
    password_hash VARCHAR(255) NOT NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'pending', -- pending | approved | rejected
    reviewed_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    reviewed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT signup_requests_status_check CHECK (status IN ('pending', 'approved', 'rejected'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_signup_requests_username_pending
ON signup_requests (username_key)
WHERE status = 'pending';

CREATE UNIQUE INDEX IF NOT EXISTS idx_signup_requests_email_pending
ON signup_requests (email_key)
WHERE email IS NOT NULL AND status = 'pending';

CREATE INDEX IF NOT EXISTS idx_signup_requests_status_created_at
ON signup_requests (status, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_signup_requests_processed_hash ON signup_requests(id) WHERE status <> 'pending' AND password_hash <> '';

--------------------------------------------------------------------------------
-- PART 3: AUTHENTICATION & SESSIONS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS oauth_accounts (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider VARCHAR(50) NOT NULL,
    provider_user_id VARCHAR(255) NOT NULL,
    access_token TEXT,
    refresh_token TEXT,
    token_expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(provider, provider_user_id)
);

CREATE INDEX IF NOT EXISTS idx_oauth_accounts_user_id ON oauth_accounts(user_id);
CREATE INDEX IF NOT EXISTS idx_oauth_accounts_provider ON oauth_accounts(provider, provider_user_id);

CREATE TABLE IF NOT EXISTS sessions (
    id VARCHAR(64) PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_agent TEXT,
    ip_address INET,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    csrf_token VARCHAR(64),
    csrf_expires_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_sessions_user_id ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expires_at ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS anonymous_sessions (
    id VARCHAR(64) PRIMARY KEY,
    user_agent TEXT,
    ip_address INET,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    csrf_token VARCHAR(64),
    csrf_expires_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_anonymous_sessions_expires_at ON anonymous_sessions(expires_at);

CREATE TABLE IF NOT EXISTS oauth_states (
    state VARCHAR(64) PRIMARY KEY,
    pkce_verifier VARCHAR(128) NOT NULL,
    provider VARCHAR(50) NOT NULL,
    redirect_uri TEXT,
    return_to TEXT NOT NULL DEFAULT '/',
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    browser_binding VARCHAR(64)
);

CREATE INDEX IF NOT EXISTS idx_oauth_states_expires_at ON oauth_states(expires_at);

--------------------------------------------------------------------------------
-- PART 4: SSH & GPG KEYS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS personal_access_tokens (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name VARCHAR(100) NOT NULL CHECK (length(name) > 0),
    token_hash VARCHAR(64) NOT NULL UNIQUE,
    scope TEXT NOT NULL CHECK (scope IN ('read', 'write')),
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_personal_access_tokens_user ON personal_access_tokens(user_id);

CREATE TABLE IF NOT EXISTS ssh_keys (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    fingerprint VARCHAR(255) NOT NULL,
    public_key TEXT NOT NULL,
    key_type VARCHAR(50) NOT NULL,
    last_used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT ssh_keys_fingerprint_unique UNIQUE (fingerprint)
);

CREATE INDEX IF NOT EXISTS idx_ssh_keys_user_id ON ssh_keys(user_id);

CREATE TABLE IF NOT EXISTS gpg_keys (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key_id VARCHAR(40) NOT NULL UNIQUE,
    public_key TEXT NOT NULL,
    emails TEXT[] NOT NULL DEFAULT '{}',
    can_sign BOOLEAN NOT NULL DEFAULT TRUE,
    can_encrypt BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at TIMESTAMPTZ,
    verified BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_gpg_keys_user_id ON gpg_keys(user_id);
CREATE INDEX IF NOT EXISTS idx_gpg_keys_key_id ON gpg_keys(key_id);

--------------------------------------------------------------------------------
-- PART 5: TWO-FACTOR AUTHENTICATION
--------------------------------------------------------------------------------

-- The encrypted TOTP secret, the last accepted time step (a code works once) and the hashed
-- one-time recovery codes. users.two_factor_enabled is kept in step.
CREATE TABLE two_factor_auth (
    user_id BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    totp_secret_encrypted BYTEA NOT NULL,
    totp_nonce BYTEA NOT NULL,
    last_used_step BIGINT,
    enabled_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE two_factor_recovery_codes (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL CHECK (char_length(code_hash) = 64),
    PRIMARY KEY (user_id, code_hash)
);

--------------------------------------------------------------------------------
-- PART 6: ORGANIZATIONS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS organizations (
    id BIGSERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL UNIQUE,
    display_name VARCHAR(255),
    description TEXT,
    avatar_file_id UUID REFERENCES files(id) ON DELETE SET NULL,
    billing_email VARCHAR(255),
    is_verified BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_organizations_name ON organizations(name);
CREATE INDEX IF NOT EXISTS idx_organizations_avatar_file ON organizations(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS org_memberships (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(20) NOT NULL DEFAULT 'member', -- owner, admin, member
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(org_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_org_memberships_org_id ON org_memberships(org_id);
CREATE INDEX IF NOT EXISTS idx_org_memberships_user_id ON org_memberships(user_id);

-- Organization invitations: a pending offer of membership. Membership starts only when the
-- invitee accepts; a decline or a cancellation deletes the row.
CREATE TABLE IF NOT EXISTS organization_invitations (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(20) NOT NULL DEFAULT 'member', -- owner, admin, member
    invited_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(org_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_organization_invitations_user_id ON organization_invitations(user_id);

--------------------------------------------------------------------------------
-- PART 7: TEAMS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS teams (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    slug VARCHAR(255) NOT NULL,
    description TEXT,
    privacy VARCHAR(20) NOT NULL DEFAULT 'visible', -- visible, secret
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(org_id, slug)
);

CREATE INDEX IF NOT EXISTS idx_teams_org_id ON teams(org_id);

CREATE TABLE IF NOT EXISTS team_memberships (
    id BIGSERIAL PRIMARY KEY,
    team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(20) NOT NULL DEFAULT 'member', -- maintainer, member
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(team_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_team_memberships_team_id ON team_memberships(team_id);
CREATE INDEX IF NOT EXISTS idx_team_memberships_user_id ON team_memberships(user_id);

--------------------------------------------------------------------------------
-- PART 8: REPOSITORIES
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS repositories (
    id BIGSERIAL PRIMARY KEY,
    owner_kind VARCHAR(10) NOT NULL DEFAULT 'user', -- user, org
    owner_user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    description TEXT,
    website VARCHAR(255),
    is_private BOOLEAN NOT NULL DEFAULT FALSE,
    is_archived BOOLEAN NOT NULL DEFAULT FALSE,
    is_fork BOOLEAN NOT NULL DEFAULT FALSE,
    forked_from_id BIGINT REFERENCES repositories(id) ON DELETE SET NULL,
    -- Canonical Git refs can contain 1024 bytes.
    default_branch VARCHAR(1024) NOT NULL DEFAULT 'main',
    milestone_number INTEGER NOT NULL DEFAULT 0 CHECK (milestone_number >= 0),
    disk_path TEXT NOT NULL,
    storage_node_id BIGINT,
    size_bytes BIGINT NOT NULL DEFAULT 0,
    stars_count INTEGER NOT NULL DEFAULT 0,
    forks_count INTEGER NOT NULL DEFAULT 0,
    watchers_count INTEGER NOT NULL DEFAULT 0,
    open_issues_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Set on forks of private repositories: they can never be made public.
    visibility_locked BOOLEAN NOT NULL DEFAULT FALSE,
    -- Counts the ref changes made on the primary. A replica is current when its
    -- applied_version equals this.
    write_version BIGINT NOT NULL DEFAULT 0 CHECK (write_version >= 0),
    allow_merge_commit BOOLEAN NOT NULL DEFAULT TRUE,
    allow_squash_merge BOOLEAN NOT NULL DEFAULT TRUE,
    allow_rebase_merge BOOLEAN NOT NULL DEFAULT TRUE,
    -- The user who created the repository, so an organization repository is listed under
    -- its creator. Historical attribution can outlive the account; no creator falls back to the owner.
    created_by_id BIGINT,
    CONSTRAINT repo_owner_check CHECK (
        (owner_kind = 'user' AND owner_user_id IS NOT NULL AND owner_org_id IS NULL) OR
        (owner_kind = 'org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL)
    ),
    CONSTRAINT repo_unique_user UNIQUE (owner_user_id, name),
    CONSTRAINT repo_unique_org UNIQUE (owner_org_id, name)
);

CREATE INDEX IF NOT EXISTS idx_repositories_owner_user_id ON repositories(owner_user_id);
CREATE INDEX IF NOT EXISTS idx_repositories_owner_org_id ON repositories(owner_org_id);
CREATE INDEX IF NOT EXISTS idx_repositories_storage_node ON repositories(storage_node_id, id);
CREATE INDEX IF NOT EXISTS idx_repositories_name ON repositories(name);
CREATE INDEX IF NOT EXISTS idx_repositories_storage_node ON repositories(storage_node_id, id);
CREATE INDEX IF NOT EXISTS idx_repositories_storage_uuid ON repositories (right(disk_path, 40));
CREATE INDEX IF NOT EXISTS idx_repositories_is_private ON repositories(is_private);
CREATE INDEX IF NOT EXISTS idx_repositories_forked_from_id ON repositories(forked_from_id);
CREATE INDEX IF NOT EXISTS idx_repositories_created_at ON repositories(created_at);
CREATE INDEX IF NOT EXISTS idx_repositories_updated_at ON repositories(updated_at);
CREATE INDEX IF NOT EXISTS idx_repositories_created_by ON repositories(created_by_id);
CREATE INDEX IF NOT EXISTS idx_repositories_public_stars
    ON repositories(stars_count DESC, created_at DESC, id DESC) WHERE is_private = false;
CREATE INDEX IF NOT EXISTS idx_repositories_search_order
    ON repositories(stars_count DESC, updated_at DESC);

-- User-level repo access
CREATE TABLE IF NOT EXISTS repo_access (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    access_level SMALLINT NOT NULL DEFAULT 1, -- 0=none, 1=read, 2=triage, 3=write, 4=maintain, 5=admin
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, repo_id)
);

CREATE INDEX IF NOT EXISTS idx_repo_access_user_id ON repo_access(user_id);
CREATE INDEX IF NOT EXISTS idx_repo_access_repo_id ON repo_access(repo_id);

-- Team-level repo access
CREATE TABLE IF NOT EXISTS team_repo_access (
    id BIGSERIAL PRIMARY KEY,
    team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    permission SMALLINT NOT NULL DEFAULT 1, -- 1=read, 2=triage, 3=write, 4=maintain, 5=admin
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(team_id, repo_id)
);

CREATE INDEX IF NOT EXISTS idx_team_repo_access_team_id ON team_repo_access(team_id);
CREATE INDEX IF NOT EXISTS idx_team_repo_access_repo_id ON team_repo_access(repo_id);

-- Deploy keys (repo-specific SSH keys)
CREATE TABLE IF NOT EXISTS deploy_keys (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    fingerprint VARCHAR(255) NOT NULL,
    public_key TEXT NOT NULL,
    read_only BOOLEAN NOT NULL DEFAULT TRUE,
    last_used_at TIMESTAMPTZ,
    -- The account that added the key, which its pushes are attributed to.
    created_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, fingerprint)
);

CREATE INDEX IF NOT EXISTS idx_deploy_keys_repo_id ON deploy_keys(repo_id);
CREATE INDEX IF NOT EXISTS idx_deploy_keys_fingerprint ON deploy_keys(fingerprint);

--------------------------------------------------------------------------------
-- PART 9: LANGUAGE STATS & CONTRIBUTORS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS repo_language_cache (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    storage_node_id BIGINT,
    write_version BIGINT NOT NULL,
    revision TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS repo_languages (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    language VARCHAR(100) NOT NULL,
    bytes BIGINT NOT NULL,
    percentage DOUBLE PRECISION NOT NULL CHECK (percentage BETWEEN -999.99 AND 999.99),
    color VARCHAR(7),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, language)
);

CREATE INDEX IF NOT EXISTS idx_repo_languages_repo_id ON repo_languages(repo_id);

-- Topics group repositories on Explore. Writes keep at most 20 per repository, a count that a
-- constraint cannot check.
CREATE TABLE IF NOT EXISTS repository_topics (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    topic VARCHAR(35) NOT NULL CHECK (topic ~ '^[a-z0-9-]{1,35}$'),
    PRIMARY KEY (repo_id, topic)
);

CREATE INDEX IF NOT EXISTS idx_repository_topics_topic ON repository_topics(topic);

CREATE TABLE IF NOT EXISTS repo_contributors (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    email VARCHAR(255) NOT NULL,
    name VARCHAR(255) NOT NULL,
    commits_count INTEGER NOT NULL DEFAULT 0,
    additions INTEGER NOT NULL DEFAULT 0,
    deletions INTEGER NOT NULL DEFAULT 0,
    avatar_url TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, email)
);

CREATE INDEX IF NOT EXISTS idx_repo_contributors_repo_id ON repo_contributors(repo_id);
CREATE INDEX IF NOT EXISTS idx_repo_contributors_user_id ON repo_contributors(user_id);
CREATE INDEX IF NOT EXISTS idx_repo_contributors_updated_at ON repo_contributors(updated_at);

CREATE TABLE IF NOT EXISTS repo_storage_history (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    size_bytes BIGINT NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_repo_storage_history_repo_time ON repo_storage_history(repo_id, recorded_at DESC);
CREATE INDEX IF NOT EXISTS idx_repo_storage_history_recorded_at ON repo_storage_history(recorded_at);

-- Branch updates made by pushes, the web editor, the API and merges, for profile activity.
-- Each row is one branch update. Commit counts come from Git's own range count.
CREATE TABLE IF NOT EXISTS repository_pushes (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    branch VARCHAR(1024) NOT NULL,
    commit_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_repository_pushes_actor ON repository_pushes(actor_id, created_at);
CREATE INDEX IF NOT EXISTS idx_repository_pushes_created_at ON repository_pushes(created_at);

--------------------------------------------------------------------------------
-- PART 10: BRANCH PROTECTION
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS branch_protection_rules (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pattern VARCHAR(255) NOT NULL, -- branch name or pattern like 'main', 'release/*'
    require_pull_request BOOLEAN NOT NULL DEFAULT FALSE,
    required_approving_reviews INTEGER NOT NULL DEFAULT 0,
    dismiss_stale_reviews BOOLEAN NOT NULL DEFAULT FALSE,
    require_code_owner_review BOOLEAN NOT NULL DEFAULT FALSE,
    require_status_checks BOOLEAN NOT NULL DEFAULT FALSE,
    required_status_checks TEXT[] DEFAULT '{}',
    require_up_to_date_branch BOOLEAN NOT NULL DEFAULT FALSE,
    require_linear_history BOOLEAN NOT NULL DEFAULT FALSE,
    block_force_pushes BOOLEAN NOT NULL DEFAULT TRUE,
    block_deletions BOOLEAN NOT NULL DEFAULT TRUE,
    restrict_pushes BOOLEAN NOT NULL DEFAULT FALSE,
    push_allowances BIGINT[] DEFAULT '{}', -- user IDs allowed to push
    -- User IDs allowed to force push when block_force_pushes is set.
    force_push_allowances BIGINT[] DEFAULT '{}',
    enforce_admins BOOLEAN NOT NULL DEFAULT TRUE,
    require_conversation_resolution BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, pattern)
);

CREATE INDEX IF NOT EXISTS idx_branch_protection_rules_repo_id ON branch_protection_rules(repo_id);

CREATE TABLE IF NOT EXISTS commit_statuses (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    sha VARCHAR(40) NOT NULL,
    context TEXT NOT NULL,
    state VARCHAR(20) NOT NULL, -- pending, success, failure, error
    description TEXT,
    target_url TEXT,
    creator_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, sha, context)
);

CREATE INDEX IF NOT EXISTS idx_commit_statuses_repo_sha ON commit_statuses(repo_id, sha);

--------------------------------------------------------------------------------
-- PART 11: LABELS & MILESTONES
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS labels (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name VARCHAR(50) NOT NULL,
    color VARCHAR(7) NOT NULL DEFAULT '#6b7280',
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, name)
);

CREATE INDEX IF NOT EXISTS idx_labels_repo_id ON labels(repo_id);

CREATE TABLE IF NOT EXISTS milestones (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    title VARCHAR(255) NOT NULL,
    description TEXT,
    due_date TIMESTAMPTZ,
    state VARCHAR(10) NOT NULL DEFAULT 'open',
    open_issues INTEGER NOT NULL DEFAULT 0,
    closed_issues INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at TIMESTAMPTZ,
    UNIQUE(repo_id, number)
);

CREATE INDEX IF NOT EXISTS idx_milestones_repo_id ON milestones(repo_id);

--------------------------------------------------------------------------------
-- PART 12: ISSUES
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS issues (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    author_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title VARCHAR(255) NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state VARCHAR(10) NOT NULL DEFAULT 'open',
    state_reason VARCHAR(20), -- completed, not_planned, reopened
    locked BOOLEAN NOT NULL DEFAULT FALSE,
    lock_reason VARCHAR(20),
    milestone_id BIGINT REFERENCES milestones(id) ON DELETE SET NULL,
    closed_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at TIMESTAMPTZ,
    UNIQUE(repo_id, number)
);

CREATE INDEX IF NOT EXISTS idx_issues_repo_id ON issues(repo_id);
CREATE INDEX IF NOT EXISTS idx_issues_author_id ON issues(author_id);
CREATE INDEX IF NOT EXISTS idx_issues_state ON issues(state);
CREATE INDEX IF NOT EXISTS idx_issues_repo_state ON issues(repo_id, state);
CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_issues_milestone_id ON issues(milestone_id);

CREATE TABLE IF NOT EXISTS issue_labels (
    issue_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    label_id BIGINT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
);

CREATE TABLE IF NOT EXISTS issue_assignees (
    issue_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, user_id)
);

CREATE TABLE IF NOT EXISTS issue_comments (
    id BIGSERIAL PRIMARY KEY,
    issue_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_issue_comments_issue_id ON issue_comments(issue_id);
CREATE INDEX IF NOT EXISTS idx_issue_comments_author_id ON issue_comments(author_id);
CREATE INDEX IF NOT EXISTS idx_issue_comments_created_at ON issue_comments(created_at);

--------------------------------------------------------------------------------
-- PART 13: PULL REQUESTS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS pull_requests (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    author_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title VARCHAR(255) NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state VARCHAR(10) NOT NULL DEFAULT 'open',
    head_repo_id BIGINT REFERENCES repositories(id) ON DELETE SET NULL,
    workflows_approved_sha VARCHAR(40),
    head_branch VARCHAR(255) NOT NULL,
    head_sha VARCHAR(40) NOT NULL,
    base_branch VARCHAR(255) NOT NULL,
    base_sha VARCHAR(40) NOT NULL,
    -- The base a stacked layer was built on, kept while its base branch moves on. NULL means base_sha is that base.
    fork_sha VARCHAR(40),
    is_draft BOOLEAN NOT NULL DEFAULT FALSE,
    mergeable BOOLEAN,
    merged BOOLEAN NOT NULL DEFAULT FALSE,
    merged_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    merge_commit_sha VARCHAR(40),
    stack_id BIGINT,
    milestone_id BIGINT REFERENCES milestones(id) ON DELETE SET NULL,
    additions INTEGER NOT NULL DEFAULT 0,
    deletions INTEGER NOT NULL DEFAULT 0,
    changed_files INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    merged_at TIMESTAMPTZ,
    closed_at TIMESTAMPTZ,
    UNIQUE(repo_id, number)
);

CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_id ON pull_requests(repo_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_author_id ON pull_requests(author_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_merged_by ON pull_requests(merged_by_id, merged_at) WHERE merged_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_pull_requests_state ON pull_requests(state);
CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_state ON pull_requests(repo_id, state);
CREATE INDEX IF NOT EXISTS idx_pull_requests_head_repo_state ON pull_requests(head_repo_id, state);
CREATE INDEX IF NOT EXISTS idx_pull_requests_created_at ON pull_requests(created_at);
CREATE INDEX IF NOT EXISTS idx_pull_requests_updated_at ON pull_requests(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_stack ON pull_requests(repo_id, stack_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_milestone_id ON pull_requests(milestone_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_merged_by ON pull_requests(merged_by_id, merged_at) WHERE merged_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_issue_labels_label ON issue_labels(label_id);

CREATE TABLE IF NOT EXISTS pr_labels (
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    label_id BIGINT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (pull_request_id, label_id)
);

CREATE INDEX IF NOT EXISTS idx_pr_labels_label ON pr_labels(label_id);

CREATE TABLE IF NOT EXISTS pr_assignees (
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (pull_request_id, user_id)
);

CREATE TABLE IF NOT EXISTS pr_reviewers (
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (pull_request_id, user_id)
);

-- What changed on an issue or pull request, shown between its comments.
CREATE TABLE IF NOT EXISTS timeline_events (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subject_kind VARCHAR(20) NOT NULL CHECK (subject_kind IN ('issue','pull_request')),
    subject_id BIGINT NOT NULL,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    kind VARCHAR(20) NOT NULL,
    detail VARCHAR(255) NOT NULL DEFAULT '',
    previous VARCHAR(255) NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_timeline_events_subject ON timeline_events(subject_kind, subject_id, created_at);
CREATE INDEX IF NOT EXISTS idx_timeline_events_repo_id ON timeline_events(repo_id);

CREATE TABLE IF NOT EXISTS reactions (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    subject_kind VARCHAR(20) NOT NULL CHECK (subject_kind IN ('issue','issue_comment','pull_request','pr_comment','pr_review')),
    subject_id BIGINT NOT NULL,
    content VARCHAR(10) NOT NULL CHECK (content IN ('+1','-1','laugh','hooray','confused','heart','rocket','eyes')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, subject_kind, subject_id, content)
);
CREATE INDEX IF NOT EXISTS idx_reactions_subject ON reactions(subject_kind, subject_id);
CREATE INDEX IF NOT EXISTS idx_reactions_repo_id ON reactions(repo_id);

CREATE TABLE IF NOT EXISTS pr_reviews (
    id BIGSERIAL PRIMARY KEY,
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    state VARCHAR(20) NOT NULL DEFAULT 'pending', -- pending, approved, changes_requested, commented, dismissed
    body TEXT,
    commit_sha VARCHAR(40),
    dismissed_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    dismissed_reason TEXT,
    dismissed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_pr_reviews_pr_id ON pr_reviews(pull_request_id);
CREATE INDEX IF NOT EXISTS idx_pr_reviews_author_id ON pr_reviews(author_id);
CREATE INDEX IF NOT EXISTS idx_pr_reviews_created_at ON pr_reviews(created_at);

-- Each author has at most one pending review on a pull request.
CREATE UNIQUE INDEX IF NOT EXISTS idx_pr_reviews_one_pending ON pr_reviews(pull_request_id, author_id) WHERE state = 'pending';

CREATE TABLE IF NOT EXISTS pr_comments (
    id BIGSERIAL PRIMARY KEY,
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    path TEXT,
    line INTEGER,
    side VARCHAR(5), -- LEFT, RIGHT
    commit_sha VARCHAR(40),
    in_reply_to_comment_id BIGINT REFERENCES pr_comments(id) ON DELETE CASCADE,
    resolved_at TIMESTAMPTZ,
    resolved_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    -- The review this comment was filed in. A pending review's comments stay private to its author.
    review_id BIGINT REFERENCES pr_reviews(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_pr_comments_pr_id ON pr_comments(pull_request_id);
CREATE INDEX IF NOT EXISTS idx_pr_comments_author_id ON pr_comments(author_id);
CREATE INDEX IF NOT EXISTS idx_pr_comments_created_at ON pr_comments(created_at);

CREATE INDEX IF NOT EXISTS idx_pr_comments_reply_to ON pr_comments(in_reply_to_comment_id);
CREATE INDEX IF NOT EXISTS idx_pr_comments_review_id ON pr_comments(review_id);

-- Files a reader marked as viewed on a pull request. The mark keeps the file's blob id, so a
-- later push that changes the file leaves it unviewed.
CREATE TABLE IF NOT EXISTS pull_request_viewed_files (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    blob_sha TEXT NOT NULL,
    PRIMARY KEY (user_id, pull_request_id, path)
);

--------------------------------------------------------------------------------
-- PART 14: RELEASES
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS releases (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    tag_name VARCHAR(255) NOT NULL,
    target_commitish VARCHAR(255) NOT NULL DEFAULT 'main',
    name VARCHAR(255),
    body TEXT,
    is_draft BOOLEAN NOT NULL DEFAULT FALSE,
    is_prerelease BOOLEAN NOT NULL DEFAULT FALSE,
    is_latest BOOLEAN NOT NULL DEFAULT FALSE,
    author_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_at TIMESTAMPTZ,
    UNIQUE(repo_id, tag_name)
);

CREATE INDEX IF NOT EXISTS idx_releases_repo_id ON releases(repo_id);
CREATE INDEX IF NOT EXISTS idx_releases_author_id ON releases(author_id);
CREATE INDEX IF NOT EXISTS idx_releases_created_at ON releases(created_at);
-- One latest release per repository.
CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_one_latest ON releases(repo_id) WHERE is_latest;

CREATE TABLE IF NOT EXISTS release_assets (
    id BIGSERIAL PRIMARY KEY,
    release_id BIGINT NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    file_id UUID NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    label VARCHAR(255),
    content_type VARCHAR(100) NOT NULL,
    size BIGINT NOT NULL,
    download_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_release_assets_release_id ON release_assets(release_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_release_assets_name ON release_assets(release_id, name);
CREATE INDEX IF NOT EXISTS idx_release_assets_file_id ON release_assets(file_id);

--------------------------------------------------------------------------------
-- PART 15: SOCIAL (Stars, Watches, Follows)
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS stars (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, repo_id)
);

CREATE INDEX IF NOT EXISTS idx_stars_repo_id ON stars(repo_id);

CREATE TABLE IF NOT EXISTS watches (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    level VARCHAR(20) NOT NULL DEFAULT 'watching', -- watching, custom, ignoring
    -- Comma-separated events a custom watch wants: issues, pulls.
    events VARCHAR(40) NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, repo_id)
);

CREATE INDEX IF NOT EXISTS idx_watches_repo_id ON watches(repo_id);

CREATE TABLE IF NOT EXISTS follows (
    follower_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    following_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (follower_id, following_id),
    CHECK (follower_id != following_id)
);

CREATE INDEX IF NOT EXISTS idx_follows_follower_id ON follows(follower_id);
CREATE INDEX IF NOT EXISTS idx_follows_following_id ON follows(following_id);

--------------------------------------------------------------------------------
-- PART 16: USER PROFILE EXTRAS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS pinned_repos (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    position SMALLINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, repo_id)
);

CREATE INDEX IF NOT EXISTS idx_pinned_repos_user_id ON pinned_repos(user_id);

--------------------------------------------------------------------------------
-- PART 17: SECRETS (encrypted)
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS repo_secrets (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    encrypted_value BYTEA NOT NULL,
    nonce BYTEA NOT NULL,
    created_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, name)
);

CREATE INDEX IF NOT EXISTS idx_repo_secrets_repo_id ON repo_secrets(repo_id);

CREATE TABLE IF NOT EXISTS org_secrets (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    encrypted_value BYTEA NOT NULL,
    nonce BYTEA NOT NULL,
    visibility VARCHAR(20) NOT NULL DEFAULT 'all', -- all, private, selected
    created_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(org_id, name)
);

CREATE INDEX IF NOT EXISTS idx_org_secrets_org_id ON org_secrets(org_id);

--------------------------------------------------------------------------------
-- PART 18: AUDIT LOG
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS audit_log (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    action VARCHAR(100) NOT NULL,
    target_type VARCHAR(50),
    target_id BIGINT,
    metadata JSONB DEFAULT '{}',
    ip_address INET,
    user_agent TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_audit_log_org_id ON audit_log(org_id);
CREATE INDEX IF NOT EXISTS idx_audit_log_actor_id ON audit_log(actor_id);
CREATE INDEX IF NOT EXISTS idx_audit_log_action ON audit_log(action);
CREATE INDEX IF NOT EXISTS idx_audit_log_created_at ON audit_log(created_at);

--------------------------------------------------------------------------------
-- PART 19: RATE LIMITING
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS upload_rate_limit (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    file_id UUID REFERENCES files(id) ON DELETE SET NULL,
    file_size BIGINT NOT NULL,
    reason VARCHAR(50),
    ip_address INET,
    uploaded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_user_id ON upload_rate_limit(user_id);
CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_uploaded_at ON upload_rate_limit(uploaded_at);
CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_reason ON upload_rate_limit(reason);
CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_ip ON upload_rate_limit(ip_address);
CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_file_id ON upload_rate_limit(file_id);
CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_user_reason_time ON upload_rate_limit(user_id, reason, uploaded_at);

CREATE TABLE IF NOT EXISTS download_rate_limit (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    file_id UUID NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    reason VARCHAR(50),
    ip_address INET,
    downloaded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_download_rate_limit_user_id ON download_rate_limit(user_id);
CREATE INDEX IF NOT EXISTS idx_download_rate_limit_downloaded_at ON download_rate_limit(downloaded_at);
CREATE INDEX IF NOT EXISTS idx_download_rate_limit_reason ON download_rate_limit(reason);
CREATE INDEX IF NOT EXISTS idx_download_rate_limit_ip ON download_rate_limit(ip_address);
CREATE INDEX IF NOT EXISTS idx_download_rate_limit_file_id ON download_rate_limit(file_id);

--------------------------------------------------------------------------------
-- PART 20: NOTIFICATIONS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS notifications (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE SET NULL,
    kind VARCHAR(64) NOT NULL,
    title TEXT NOT NULL,
    body TEXT,
    link TEXT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    read_at TIMESTAMPTZ,
    -- Saved for later, and done (out of the inbox). NULL means not set.
    saved_at TIMESTAMPTZ,
    done_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Repository the notification was about, kept after the repository is gone
    -- so its notifications stay hidden; 0 = not about a repository.
    scope_repository_id BIGINT NOT NULL CHECK (scope_repository_id >= 0)
);

CREATE INDEX IF NOT EXISTS idx_notifications_scope ON notifications(user_id, scope_repository_id);
CREATE INDEX IF NOT EXISTS idx_notifications_user_created_at ON notifications(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_user_unread ON notifications(user_id, read_at, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_user_saved ON notifications(user_id, saved_at, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_user_done ON notifications(user_id, done_at, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_actor_id ON notifications(actor_id);
CREATE INDEX IF NOT EXISTS idx_notifications_repo_id ON notifications(repo_id);

--------------------------------------------------------------------------------
-- PART 21: FUNCTIONS & TRIGGERS
--------------------------------------------------------------------------------

-- Update stars count
CREATE OR REPLACE FUNCTION update_repo_stars_count() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE repositories SET stars_count = stars_count + 1 WHERE id = NEW.repo_id;
    ELSIF TG_OP = 'DELETE' THEN
        UPDATE repositories SET stars_count = stars_count - 1 WHERE id = OLD.repo_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trigger_update_stars_count ON stars;
CREATE TRIGGER trigger_update_stars_count
    AFTER INSERT OR DELETE ON stars
    FOR EACH ROW EXECUTE FUNCTION update_repo_stars_count();

-- Update watchers count. Ignoring a repository does not count as watching it.
CREATE OR REPLACE FUNCTION update_repo_watchers_count() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP IN ('DELETE', 'UPDATE') AND OLD.level != 'ignoring' THEN
        UPDATE repositories SET watchers_count = watchers_count - 1 WHERE id = OLD.repo_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') AND NEW.level != 'ignoring' THEN
        UPDATE repositories SET watchers_count = watchers_count + 1 WHERE id = NEW.repo_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trigger_update_watchers_count ON watches;
CREATE TRIGGER trigger_update_watchers_count
    AFTER INSERT OR DELETE OR UPDATE OF level, repo_id ON watches
    FOR EACH ROW EXECUTE FUNCTION update_repo_watchers_count();

-- Update forks count
CREATE OR REPLACE FUNCTION update_repo_forks_count() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'INSERT' AND NEW.forked_from_id IS NOT NULL THEN
        UPDATE repositories SET forks_count = forks_count + 1 WHERE id = NEW.forked_from_id;
    ELSIF TG_OP = 'DELETE' AND OLD.forked_from_id IS NOT NULL THEN
        UPDATE repositories SET forks_count = forks_count - 1 WHERE id = OLD.forked_from_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trigger_update_forks_count ON repositories;
CREATE TRIGGER trigger_update_forks_count
    AFTER INSERT OR DELETE ON repositories
    FOR EACH ROW EXECUTE FUNCTION update_repo_forks_count();

-- Get next issue/PR number
-- Issues and pull requests share one number sequence per repository.
CREATE TABLE IF NOT EXISTS repository_discussion_counters (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    next_number BIGINT NOT NULL CHECK (next_number BETWEEN 1 AND 2147483648)
);

CREATE OR REPLACE FUNCTION get_next_issue_number(p_repo_id BIGINT) RETURNS INTEGER AS $$
DECLARE allocated BIGINT;
BEGIN
    INSERT INTO repository_discussion_counters (repo_id, next_number) VALUES (p_repo_id, 2)
    ON CONFLICT (repo_id) DO UPDATE SET next_number = repository_discussion_counters.next_number + 1
    RETURNING next_number - 1 INTO allocated;
    IF allocated > 2147483647 THEN
        RAISE EXCEPTION 'Repository discussion number space exhausted' USING ERRCODE = '22003';
    END IF;
    RETURN allocated::INTEGER;
END;
$$ LANGUAGE plpgsql;

-- Cleanup expired data
CREATE OR REPLACE FUNCTION cleanup_expired_data() RETURNS void AS $$
BEGIN
    DELETE FROM sessions WHERE expires_at < NOW();
    DELETE FROM anonymous_sessions WHERE expires_at < NOW();
    DELETE FROM oauth_states WHERE expires_at < NOW();
END;
$$ LANGUAGE plpgsql;

-- Cleanup function for old rate limit records
CREATE OR REPLACE FUNCTION cleanup_old_rate_limits() RETURNS void AS $$
BEGIN
    DELETE FROM upload_rate_limit WHERE uploaded_at < NOW() - INTERVAL '30 days';
    DELETE FROM download_rate_limit WHERE downloaded_at < NOW() - INTERVAL '30 days';
END;
$$ LANGUAGE plpgsql;

--------------------------------------------------------------------------------
-- PART 22: FILE MANAGEMENT VIEWS AND LIMITS
--------------------------------------------------------------------------------

-- User file stats view for quick analytics
CREATE OR REPLACE VIEW user_file_stats AS
SELECT 
    u.id as user_id,
    u.username,
    COUNT(DISTINCT f.id) as total_files,
    COALESCE(SUM(f.file_size), 0) as total_bytes,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'avatar') as avatar_count,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'release_asset') as release_asset_count,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'issue_attachment') as issue_attachment_count,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'pr_attachment') as pr_attachment_count,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'readme_image') as readme_image_count,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'repo_asset') as repo_asset_count,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'org_avatar') as org_avatar_count,
    COUNT(DISTINCT f.id) FILTER (WHERE f.reason = 'other') as other_count,
    -- Last 24h stats
    COUNT(DISTINCT ul.id) FILTER (WHERE ul.uploaded_at > NOW() - INTERVAL '24 hours') as uploads_24h,
    COALESCE(SUM(ul.file_size) FILTER (WHERE ul.uploaded_at > NOW() - INTERVAL '24 hours'), 0) as bytes_24h,
    COUNT(DISTINCT dl.id) FILTER (WHERE dl.downloaded_at > NOW() - INTERVAL '24 hours') as downloads_24h
FROM users u
LEFT JOIN files f ON f.user_id = u.id AND f.is_active = true
LEFT JOIN upload_rate_limit ul ON ul.user_id = u.id
LEFT JOIN download_rate_limit dl ON dl.user_id = u.id
GROUP BY u.id, u.username;

-- Abuse detection: users with suspicious activity
CREATE OR REPLACE VIEW suspicious_upload_activity AS
SELECT 
    user_id,
    ip_address,
    COUNT(*) as upload_count,
    SUM(file_size) as total_bytes,
    MIN(uploaded_at) as first_upload,
    MAX(uploaded_at) as last_upload,
    COUNT(DISTINCT reason) as reason_variety
FROM upload_rate_limit
WHERE uploaded_at > NOW() - INTERVAL '1 hour'
GROUP BY user_id, ip_address
HAVING COUNT(*) > 20 OR SUM(file_size) > 100 * 1024 * 1024; -- >20 uploads or >100MB/hour

-- Per-reason rate limits (configurable abuse thresholds)
CREATE TABLE IF NOT EXISTS file_reason_limits (
    reason VARCHAR(50) PRIMARY KEY,
    max_file_size_bytes BIGINT NOT NULL DEFAULT 10485760, -- 10MB default
    max_uploads_per_hour INT NOT NULL DEFAULT 10,
    max_bytes_per_hour BIGINT NOT NULL DEFAULT 104857600, -- 100MB default
    max_uploads_per_day INT NOT NULL DEFAULT 50,
    max_bytes_per_day BIGINT NOT NULL DEFAULT 524288000, -- 500MB default
    allowed_content_types TEXT[], -- NULL = all allowed
    requires_auth BOOLEAN NOT NULL DEFAULT TRUE,
    description TEXT
);

-- Insert default limits per reason
INSERT INTO file_reason_limits (reason, max_file_size_bytes, max_uploads_per_hour, max_bytes_per_hour, max_uploads_per_day, max_bytes_per_day, allowed_content_types, description) VALUES
('avatar', 2097152, 5, 10485760, 20, 41943040, ARRAY['image/jpeg', 'image/png', 'image/gif', 'image/webp'], 'User profile avatars'),
('org_avatar', 2097152, 5, 10485760, 20, 41943040, ARRAY['image/jpeg', 'image/png', 'image/gif', 'image/webp'], 'Organization avatars'),
('release_asset', 104857600, 20, 524288000, 100, 2147483648, NULL, 'Release binary assets'),
('issue_attachment', 10485760, 20, 104857600, 100, 524288000, ARRAY['image/jpeg', 'image/png', 'image/gif', 'image/webp', 'application/pdf', 'text/plain', 'text/markdown'], 'Issue attachments'),
('pr_attachment', 10485760, 20, 104857600, 100, 524288000, ARRAY['image/jpeg', 'image/png', 'image/gif', 'image/webp', 'application/pdf', 'text/plain', 'text/markdown'], 'PR attachments'),
('readme_image', 5242880, 10, 52428800, 50, 262144000, ARRAY['image/jpeg', 'image/png', 'image/gif', 'image/webp', 'image/svg+xml'], 'README and wiki images'),
('repo_asset', 52428800, 10, 262144000, 50, 1073741824, NULL, 'Repository assets'),
('other', 10485760, 5, 52428800, 20, 104857600, NULL, 'Uncategorized files')
ON CONFLICT (reason) DO NOTHING;

--------------------------------------------------------------------------------
-- PART 22: CI/CD RUNS, LOGS, AND DEPLOYMENTS
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS ci_runs (
    id BIGSERIAL PRIMARY KEY,
    run_id UUID NOT NULL UNIQUE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    triggered_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    pull_request_id BIGINT REFERENCES pull_requests(id) ON DELETE SET NULL,
    mode VARCHAR(20) NOT NULL, -- local, podman, kubernetes
    pipeline VARCHAR(20) NOT NULL DEFAULT 'ci', -- ci, cd, full
    event VARCHAR(50) NOT NULL,
    is_fork BOOLEAN NOT NULL DEFAULT FALSE,
    branch VARCHAR(255) NOT NULL,
    head_sha VARCHAR(40),
    workflow_path TEXT NOT NULL,
    environment VARCHAR(255),
    status VARCHAR(20) NOT NULL DEFAULT 'queued', -- queued, running, success, failed, cancelled
    error_message TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Exact commit and workflow file blobs the run executes.
    workflow_sha VARCHAR(40),
    workflow_blobs JSONB NOT NULL DEFAULT '{}',
    -- Worker node that claimed the run, and its last sign of life.
    worker TEXT,
    heartbeat_at TIMESTAMPTZ,
    cancel_requested BOOLEAN NOT NULL DEFAULT FALSE,
    -- Stored log totals per run. A log append checks the run limit against these
    -- columns instead of counting the run's lines on every append.
    log_lines BIGINT NOT NULL DEFAULT 0 CHECK (log_lines >= 0),
    log_bytes BIGINT NOT NULL DEFAULT 0 CHECK (log_bytes >= 0),
    -- A frozen copy of the pull request fields the run was started with.
    pull_request_event JSONB,
    -- The inputs a manual run was started with: a JSON object of strings, as GitHub's event.inputs.
    workflow_inputs JSONB,
    -- The attempt at a pull request revision: 1 for the first run, one more for each run started again.
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_ci_runs_pull_revision ON ci_runs(pull_request_id, branch, head_sha, workflow_path, attempt)
    WHERE event = 'pull_request' AND pull_request_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ci_runs_queue ON ci_runs(status, id) WHERE status IN ('queued', 'running');
CREATE INDEX IF NOT EXISTS idx_ci_runs_repo_created_at ON ci_runs(repo_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_ci_runs_repo_head ON ci_runs(repo_id, head_sha, id);
CREATE INDEX IF NOT EXISTS idx_ci_runs_user_created_at ON ci_runs(triggered_by_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_ci_runs_status ON ci_runs(status);
CREATE INDEX IF NOT EXISTS idx_ci_runs_push_created_at ON ci_runs(created_at) WHERE event = 'push' AND triggered_by_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ci_runs_push_user_created_at ON ci_runs(triggered_by_id, created_at DESC)
    WHERE event = 'push';

CREATE TABLE IF NOT EXISTS ci_logs (
    id BIGSERIAL PRIMARY KEY,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    stage VARCHAR(255) NOT NULL,
    line TEXT NOT NULL,
    level VARCHAR(20) NOT NULL DEFAULT 'info', -- info, warn, error, debug
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_ci_logs_run_id_id ON ci_logs(run_id, id);

CREATE TABLE IF NOT EXISTS ci_artifacts (
    id BIGSERIAL PRIMARY KEY,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    storage_url TEXT NOT NULL,
    size_bytes BIGINT,
    content_type VARCHAR(100),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_ci_artifacts_run_id ON ci_artifacts(run_id);

CREATE TABLE IF NOT EXISTS ci_deployments (
    id BIGSERIAL PRIMARY KEY,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    environment VARCHAR(255) NOT NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'pending', -- pending, approved, in_progress, success, failed, cancelled
    approved_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    approved_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_ci_deployments_repo_env ON ci_deployments(repo_id, environment, created_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_ci_deployments_run ON ci_deployments(run_id);

-- commit_statuses is created before ci_runs, so its link to ci_runs is added here.
ALTER TABLE commit_statuses ADD COLUMN ci_run_id BIGINT REFERENCES ci_runs(id) ON DELETE SET NULL;

-- Commits a push actually delivered, so CI can run them after a later force-push.
CREATE TABLE IF NOT EXISTS ci_accepted_pushes (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    reference TEXT NOT NULL,
    sha VARCHAR(40) NOT NULL,
    accepted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY(repo_id, actor_id, reference, sha)
);

-- Per-account login throttling.
CREATE TABLE IF NOT EXISTS auth_login_limits (
    account_key VARCHAR(80) PRIMARY KEY,
    attempts INTEGER NOT NULL CHECK (attempts BETWEEN 0 AND 120),
    window_ends_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_auth_login_limits_expiry ON auth_login_limits (window_ends_at)
    WHERE account_key <> 'global';

--------------------------------------------------------------------------------
-- PART 23: CLUSTER MEMBERSHIP
-- Machines that may talk to each other, and the invites that admit new ones.
-- Only PostgreSQL installations run more than one node; SQLite carries the same
-- tables so both engines share one set of operations.
--------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS nodes (
    id BIGSERIAL PRIMARY KEY,
    -- Lowercase hex of the node's ed25519 public key (its iroh endpoint id).
    public_key TEXT NOT NULL UNIQUE CHECK (public_key ~ '^[0-9a-f]{64}$'),
    name TEXT NOT NULL UNIQUE CHECK (name ~ '^[a-z0-9][a-z0-9._-]{0,62}$'),
    -- Roles this node is allowed to run: web, ssh, storage, actions-worker.
    roles TEXT[] NOT NULL CHECK (cardinality(roles) > 0 AND roles <@ ARRAY['web', 'ssh', 'storage', 'actions-worker']),
    status VARCHAR(20) NOT NULL DEFAULT 'pending', -- pending, active, revoked
    -- JSON with the addresses the node last reported.
    addr_hint TEXT,
    version TEXT,
    disk_free BIGINT CHECK (disk_free IS NULL OR disk_free >= 0),
    last_seen TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT nodes_status_check CHECK (status IN ('pending', 'active', 'revoked'))
);

ALTER TABLE repositories ADD CONSTRAINT repository_storage_node FOREIGN KEY (storage_node_id) REFERENCES nodes(id);

-- Only a hash of each invite secret is stored.
CREATE TABLE IF NOT EXISTS join_tokens (
    id BIGSERIAL PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    roles TEXT[] NOT NULL CHECK (cardinality(roles) > 0 AND roles <@ ARRAY['web', 'ssh', 'storage', 'actions-worker']),
    reusable BOOLEAN NOT NULL DEFAULT FALSE,
    requires_approval BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at TIMESTAMPTZ NOT NULL,
    uses INTEGER NOT NULL DEFAULT 0 CHECK (uses >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- A copy of a repository held by another node. It goes with its repository. applied_version -1
-- means nothing has been copied yet.
CREATE TABLE IF NOT EXISTS repository_replicas (
    repository_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    node_id BIGINT NOT NULL REFERENCES nodes(id),
    applied_version BIGINT NOT NULL DEFAULT -1 CHECK (applied_version >= -1),
    synced_at TIMESTAMPTZ,
    PRIMARY KEY (repository_id, node_id)
);

CREATE INDEX IF NOT EXISTS idx_repository_replicas_node ON repository_replicas(node_id);

-- Forks of private repositories stay private, including forks of forks.
CREATE OR REPLACE FUNCTION enforce_repository_visibility_floor() RETURNS TRIGGER AS $$
DECLARE source_nonpublic BOOLEAN;
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.visibility_locked THEN
        IF NOT NEW.is_private THEN
            RAISE EXCEPTION 'nonpublic fork visibility cannot be widened' USING ERRCODE = '23514';
        END IF;
        NEW.visibility_locked := TRUE;
    END IF;
    IF NEW.forked_from_id IS NOT NULL THEN
        SELECT is_private OR visibility_locked INTO source_nonpublic
          FROM repositories WHERE id = NEW.forked_from_id FOR SHARE;
        IF source_nonpublic THEN
            NEW.visibility_locked := TRUE;
            NEW.is_private := TRUE;
        END IF;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER repository_visibility_floor BEFORE INSERT OR UPDATE OF is_private, visibility_locked, forked_from_id
    ON repositories FOR EACH ROW EXECUTE FUNCTION enforce_repository_visibility_floor();

CREATE OR REPLACE FUNCTION narrow_descendant_fork_visibility() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.is_private AND NOT OLD.is_private THEN
        WITH RECURSIVE descendants(id) AS (
            SELECT id FROM repositories WHERE forked_from_id = NEW.id
            UNION
            SELECT r.id FROM repositories r JOIN descendants d ON r.forked_from_id = d.id
        )
        UPDATE repositories SET is_private = TRUE, visibility_locked = TRUE
          WHERE id IN (SELECT id FROM descendants) AND (NOT is_private OR NOT visibility_locked);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER repository_fork_visibility_narrowing AFTER UPDATE OF is_private ON repositories
    FOR EACH ROW EXECUTE FUNCTION narrow_descendant_fork_visibility();

-- Webhooks on a repository or an organization, their encrypted secrets, and recent deliveries.
CREATE TABLE IF NOT EXISTS webhooks (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    payload_url VARCHAR(2048) NOT NULL,
    content_type VARCHAR(64) NOT NULL CHECK (content_type IN ('application/json','application/x-www-form-urlencoded')),
    encrypted_secret BYTEA,
    secret_nonce BYTEA,
    events VARCHAR(1024) NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    ssl_verify BOOLEAN NOT NULL DEFAULT TRUE,
    created_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ((repo_id IS NULL) <> (org_id IS NULL)),
    CHECK ((encrypted_secret IS NULL) = (secret_nonce IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_webhooks_repo_id ON webhooks(repo_id);

CREATE INDEX IF NOT EXISTS idx_webhooks_org_id ON webhooks(org_id);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id BIGSERIAL PRIMARY KEY,
    webhook_id BIGINT NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    guid VARCHAR(64) NOT NULL,
    event VARCHAR(64) NOT NULL,
    action VARCHAR(64),
    status_code INTEGER,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    error VARCHAR(512),
    request_headers TEXT NOT NULL,
    request_body TEXT NOT NULL,
    response_headers TEXT,
    response_body TEXT,
    delivered_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_webhook_id ON webhook_deliveries(webhook_id, id);
