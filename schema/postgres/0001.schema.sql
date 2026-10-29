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
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    email_notifications BOOLEAN NOT NULL DEFAULT TRUE
);

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
    -- Single-node mode allows only the server and runners; `codeyard cluster enable` turns on
    -- cluster mode.
    cluster_enabled BOOLEAN NOT NULL DEFAULT FALSE,
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
    email VARCHAR(255),
    password_hash VARCHAR(255) NOT NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'pending', -- pending | approved | rejected
    reviewed_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    reviewed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    username_key TEXT NOT NULL,
    email_key TEXT,
    CONSTRAINT signup_requests_status_check CHECK (status IN ('pending', 'approved', 'rejected'))
);

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
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    browser_binding VARCHAR(64),
    return_to TEXT NOT NULL DEFAULT '/'
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

CREATE TABLE IF NOT EXISTS org_memberships (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(20) NOT NULL DEFAULT 'member', -- owner, admin, member
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    is_public BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE(org_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_org_memberships_user_id ON org_memberships(user_id);

-- Organization invitations: a pending offer of membership. Membership starts only when the
-- invitee accepts; a decline or a cancellation deletes the row.
CREATE TABLE IF NOT EXISTS organization_invitations (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(20) NOT NULL DEFAULT 'member', -- owner, admin, member
    invited_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    email VARCHAR(255),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (NOW()+INTERVAL '7 days'),
    failed_at TIMESTAMPTZ,
    failed_reason TEXT,
    UNIQUE(org_id, user_id),
    CHECK (user_id IS NOT NULL OR email IS NOT NULL)
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
    default_permission SMALLINT NOT NULL DEFAULT 1 CHECK (default_permission BETWEEN 1 AND 5),
    notifications_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    parent_team_id BIGINT REFERENCES teams(id) ON DELETE SET NULL CHECK (parent_team_id IS NULL OR parent_team_id<>id),
    UNIQUE(org_id, slug)
);

CREATE TABLE IF NOT EXISTS team_memberships (
    id BIGSERIAL PRIMARY KEY,
    team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(20) NOT NULL DEFAULT 'member', -- maintainer, member
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(team_id, user_id)
);
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
    -- its creator. Repositories with no recorded creator fall back to their owner.
    created_by_id BIGINT,
    milestone_number INTEGER NOT NULL DEFAULT 0 CHECK (milestone_number >= 0),
    is_gist BOOLEAN NOT NULL DEFAULT FALSE,
    contribution_version BIGINT NOT NULL DEFAULT 0,
    actions_engine TEXT CHECK (actions_engine IN ('codeyard-actions','act')),
    is_template BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT repo_owner_check CHECK (
        (owner_kind = 'user' AND owner_user_id IS NOT NULL AND owner_org_id IS NULL) OR
        (owner_kind = 'org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL)
    ),
    CONSTRAINT repo_unique_user UNIQUE (owner_user_id, name),
    CONSTRAINT repo_unique_org UNIQUE (owner_org_id, name)
);
CREATE INDEX IF NOT EXISTS idx_repositories_name ON repositories(name);
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
CREATE INDEX IF NOT EXISTS idx_deploy_keys_fingerprint ON deploy_keys(fingerprint);

--------------------------------------------------------------------------------
-- PART 9: LANGUAGE STATS & CONTRIBUTORS
--------------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS repo_languages (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    language VARCHAR(100) NOT NULL,
    bytes BIGINT NOT NULL,
    percentage DOUBLE PRECISION NOT NULL,
    color VARCHAR(7),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, language),
    CHECK (percentage BETWEEN -999.99 AND 999.99)
);

-- Topics group repositories on Explore. Writes keep at most 20 per repository, a count that a
-- constraint cannot check.
CREATE TABLE IF NOT EXISTS repository_topics (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    topic VARCHAR(35) NOT NULL CHECK (topic ~ '^[a-z0-9-]{1,35}$'),
    PRIMARY KEY (repo_id, topic)
);

CREATE INDEX IF NOT EXISTS idx_repository_topics_topic ON repository_topics(topic);

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
    -- Team allowances restrict existing writers; they never grant repository access.
    push_team_allowances BIGINT[] NOT NULL DEFAULT '{}',
    github_settings TEXT NOT NULL DEFAULT '{}',
    UNIQUE(repo_id, pattern)
);

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
    ci_run_id BIGINT,
    UNIQUE(repo_id, sha, context)
);

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
    updated_at TIMESTAMPTZ,
    UNIQUE(repo_id, name)
);

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
    creator_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    due_on TIMESTAMPTZ,
    UNIQUE(repo_id, number)
);

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
    stack_version BIGINT NOT NULL DEFAULT 0,
    locked BOOLEAN NOT NULL DEFAULT FALSE,
    lock_reason TEXT CHECK (lock_reason IN ('off-topic','too heated','resolved','spam')),
    maintainer_can_modify BOOLEAN NOT NULL DEFAULT FALSE,
    maintainer_edit_granted_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    source_issue_id BIGINT CHECK(source_issue_id>0),
    head_publisher_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    UNIQUE(repo_id, number)
);
CREATE INDEX IF NOT EXISTS idx_pull_requests_author_id ON pull_requests(author_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_state ON pull_requests(state);
CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_state ON pull_requests(repo_id, state);
CREATE INDEX IF NOT EXISTS idx_pull_requests_head_repo_state ON pull_requests(head_repo_id, state);
CREATE INDEX IF NOT EXISTS idx_pull_requests_created_at ON pull_requests(created_at);
CREATE INDEX IF NOT EXISTS idx_pull_requests_updated_at ON pull_requests(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_pull_requests_repo_stack ON pull_requests(repo_id, stack_id);
CREATE INDEX IF NOT EXISTS idx_pull_requests_milestone_id ON pull_requests(milestone_id);

CREATE TABLE IF NOT EXISTS pr_labels (
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    label_id BIGINT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (pull_request_id, label_id)
);

CREATE TABLE IF NOT EXISTS pr_assignees (
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (pull_request_id, user_id)
);

CREATE TABLE IF NOT EXISTS pr_reviewers (
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    as_code_owner BOOLEAN NOT NULL DEFAULT FALSE,
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
    subject_kind VARCHAR(20) NOT NULL CHECK (subject_kind IN ('issue','issue_comment','pull_request','pr_comment','pr_review','release','commit_comment')),
    subject_id BIGINT NOT NULL,
    content VARCHAR(10) NOT NULL CHECK (content IN ('+1','-1','laugh','hooray','confused','heart','rocket','eyes')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, subject_kind, subject_id, content),
    CONSTRAINT reactions_release_content_check CHECK (subject_kind<>'release' OR content NOT IN ('-1','confused'))
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
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    start_line INTEGER CHECK(start_line>0),
    start_side TEXT CHECK((start_line IS NULL AND start_side IS NULL) OR (start_line IS NOT NULL AND start_side IS NOT NULL AND start_side IN ('LEFT','RIGHT') AND path IS NOT NULL AND line IS NOT NULL AND line>0 AND side IS NOT NULL AND side IN ('LEFT','RIGHT'))),
    source_issue_comment_id BIGINT CHECK(source_issue_comment_id>0)
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
    blob_sha TEXT NOT NULL
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
    is_immutable BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE(repo_id, tag_name)
);
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
CREATE UNIQUE INDEX IF NOT EXISTS idx_release_assets_name ON release_assets(release_id, name);

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
CREATE INDEX IF NOT EXISTS idx_follows_following_id ON follows(following_id);

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

-- pulls
CREATE TABLE notification_threads (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    scope_repository_id BIGINT NOT NULL CHECK(scope_repository_id>0),
    subject_type TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id,scope_repository_id,subject_type,subject_key)
);

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
    scope_repository_id BIGINT NOT NULL CHECK (scope_repository_id >= 0),
    thread_id BIGINT REFERENCES notification_threads(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_notifications_scope ON notifications(user_id, scope_repository_id);
CREATE INDEX IF NOT EXISTS idx_notifications_user_created_at ON notifications(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_user_unread ON notifications(user_id, read_at, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_user_saved ON notifications(user_id, saved_at, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_user_done ON notifications(user_id, done_at, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_actor_id ON notifications(actor_id);

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
CREATE TRIGGER trigger_update_stars_count
    AFTER INSERT OR DELETE ON stars
    FOR EACH ROW EXECUTE FUNCTION update_repo_stars_count();

-- Update watchers count. Ignoring a repository does not count as watching it.
CREATE OR REPLACE FUNCTION update_repo_watchers_count() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP IN ('DELETE', 'UPDATE') AND OLD.level IN ('watching','custom') THEN
        UPDATE repositories SET watchers_count = watchers_count - 1 WHERE id = OLD.repo_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') AND NEW.level IN ('watching','custom') THEN
        UPDATE repositories SET watchers_count = watchers_count + 1 WHERE id = NEW.repo_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
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
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    engine TEXT CHECK (engine IN ('codeyard-actions','act')),
    workflow_run_number BIGINT,
    workflow_run_id BIGINT REFERENCES ci_runs(id) ON DELETE CASCADE,
    workflow_run_attempt INTEGER,
    log_version BIGINT NOT NULL DEFAULT 0,
    runner_labels TEXT NOT NULL DEFAULT '[]',
    runner_group TEXT,
    approval_required BOOLEAN NOT NULL DEFAULT FALSE,
    event_payload JSONB
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_ci_runs_pull_revision ON ci_runs(pull_request_id, branch, head_sha, workflow_path, attempt)
    WHERE event = 'pull_request' AND pull_request_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ci_runs_queue ON ci_runs(status, id) WHERE status IN ('queued', 'running');
CREATE INDEX IF NOT EXISTS idx_ci_runs_repo_created_at ON ci_runs(repo_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_ci_runs_user_created_at ON ci_runs(triggered_by_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_ci_runs_status ON ci_runs(status);
CREATE INDEX IF NOT EXISTS idx_ci_runs_push_user_created_at ON ci_runs(triggered_by_id, created_at DESC)
    WHERE event = 'push';

-- actions-runs: observed executions, not synthetic workflow plans
CREATE TABLE actions_jobs (
    id BIGSERIAL PRIMARY KEY,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    generation TEXT NOT NULL,
    workflow_path TEXT NOT NULL,
    workflow_name TEXT,
    matrix JSONB,
    logical_job TEXT NOT NULL,
    variant INTEGER NOT NULL CHECK(variant>=0),
    name TEXT NOT NULL,
    labels JSONB NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('queued','in_progress','completed')),
    conclusion TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    duration_ms BIGINT CHECK(duration_ms>=0),
    UNIQUE(run_id,generation,workflow_path,logical_job,variant)
);

CREATE TABLE IF NOT EXISTS ci_logs (
    id BIGSERIAL PRIMARY KEY,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    stage VARCHAR(255) NOT NULL,
    line TEXT NOT NULL,
    level VARCHAR(20) NOT NULL DEFAULT 'info', -- info, warn, error, debug
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    job_id BIGINT REFERENCES actions_jobs(id) ON DELETE SET NULL,
    step_number INTEGER
);

CREATE INDEX IF NOT EXISTS idx_ci_logs_run_id_id ON ci_logs(run_id, id);

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

-- commit_statuses is created before ci_runs, so its link to ci_runs is added here.
ALTER TABLE commit_statuses ADD FOREIGN KEY (ci_run_id) REFERENCES ci_runs(id) ON DELETE SET NULL;

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
    -- Roles this node is allowed to run: web, ssh, storage, runner.
    roles TEXT[] NOT NULL CHECK (cardinality(roles) > 0 AND roles <@ ARRAY['web', 'ssh', 'storage', 'runner']),
    status VARCHAR(20) NOT NULL DEFAULT 'pending', -- pending, active, revoked
    -- JSON with the addresses the node last reported.
    addr_hint TEXT,
    version TEXT,
    disk_free BIGINT CHECK (disk_free IS NULL OR disk_free >= 0),
    last_seen TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    runner_managed BOOLEAN NOT NULL DEFAULT false,
    CONSTRAINT nodes_status_check CHECK (status IN ('pending', 'active', 'revoked'))
);

ALTER TABLE repositories ADD CONSTRAINT repositories_storage_node_id_fkey FOREIGN KEY (storage_node_id) REFERENCES nodes(id);

-- Only a hash of each invite secret is stored.
CREATE TABLE IF NOT EXISTS join_tokens (
    id BIGSERIAL PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    roles TEXT[] NOT NULL CHECK (cardinality(roles) > 0 AND roles <@ ARRAY['web', 'ssh', 'storage', 'runner']),
    reusable BOOLEAN NOT NULL DEFAULT FALSE,
    requires_approval BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at TIMESTAMPTZ NOT NULL,
    uses INTEGER NOT NULL DEFAULT 0 CHECK (uses >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    runner_scoped BOOLEAN NOT NULL DEFAULT false
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
    username VARCHAR(1024),
    encrypted_password BYTEA,
    password_nonce BYTEA CHECK ((encrypted_password IS NULL) = (password_nonce IS NULL)),
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
    action VARCHAR(100),
    status_code INTEGER,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    error VARCHAR(512),
    request_headers TEXT NOT NULL,
    request_body TEXT NOT NULL,
    response_headers TEXT,
    response_body TEXT,
    delivered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    redelivery BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_webhook_id ON webhook_deliveries(webhook_id, id);

CREATE TABLE IF NOT EXISTS email_settings (
    id SMALLINT PRIMARY KEY CHECK (id = 1),
    host VARCHAR(255) NOT NULL CHECK (length(host) >= 1),
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    security VARCHAR(10) NOT NULL CHECK (security IN ('starttls', 'tls', 'none')),
    username VARCHAR(255) NOT NULL DEFAULT '',
    encrypted_password BYTEA,
    password_nonce BYTEA,
    from_name VARCHAR(255) NOT NULL DEFAULT '',
    from_address VARCHAR(255) NOT NULL CHECK (length(from_address) >= 1),
    reply_to VARCHAR(255) NOT NULL DEFAULT '',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ((encrypted_password IS NULL) = (password_nonce IS NULL))
);

CREATE TABLE IF NOT EXISTS email_outbox (
    id BIGSERIAL PRIMARY KEY,
    to_address VARCHAR(255) NOT NULL,
    subject VARCHAR(998) NOT NULL,
    body_text TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_error VARCHAR(1000),
    sent_at TIMESTAMPTZ,
    organization_invitation_id BIGINT REFERENCES organization_invitations(id) ON DELETE SET NULL,
    -- A notification email is sent only while its recipient can still read the notification.
    notification_id BIGINT REFERENCES notifications(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS email_tokens (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    purpose VARCHAR(10) NOT NULL CHECK (purpose IN ('verify', 'reset')),
    email VARCHAR(255) NOT NULL,
    token_hash TEXT NOT NULL UNIQUE CHECK (char_length(token_hash) = 64),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (user_id, purpose)
);

CREATE TABLE IF NOT EXISTS repo_language_cache (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    storage_node_id BIGINT,
    write_version BIGINT NOT NULL,
    revision TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS actions_settings (
    id SMALLINT PRIMARY KEY CHECK (id = 1),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    run_timeout_minutes INTEGER CHECK (run_timeout_minutes BETWEEN 1 AND 10080),
    job_cpus INTEGER CHECK (job_cpus BETWEEN 1 AND 1024),
    job_memory_gib INTEGER CHECK (job_memory_gib BETWEEN 1 AND 4096),
    runner_image VARCHAR(255) CHECK (length(runner_image) >= 1),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    results_url TEXT,
    cache_size_gib INTEGER CHECK (cache_size_gib BETWEEN 1 AND 10240),
    cache_retention_days INTEGER CHECK (cache_retention_days BETWEEN 1 AND 365),
    artifact_retention_days INTEGER CHECK (artifact_retention_days BETWEEN 1 AND 365),
    default_workflow_permissions TEXT NOT NULL DEFAULT 'restricted' CHECK (default_workflow_permissions IN ('restricted','permissive')),
    codeyard_actions_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    act_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    default_engine TEXT NOT NULL DEFAULT 'codeyard-actions' CHECK ((default_engine='codeyard-actions' AND codeyard_actions_enabled) OR (default_engine='act' AND act_enabled))
);

CREATE TABLE IF NOT EXISTS repo_actions_settings (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    run_timeout_minutes INTEGER CHECK (run_timeout_minutes BETWEEN 1 AND 10080),
    job_cpus INTEGER CHECK (job_cpus BETWEEN 1 AND 1024),
    job_memory_gib INTEGER CHECK (job_memory_gib BETWEEN 1 AND 4096),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    cache_size_gib INTEGER CHECK (cache_size_gib BETWEEN 1 AND 10240),
    cache_retention_days INTEGER CHECK (cache_retention_days BETWEEN 1 AND 365),
    rest_settings TEXT NOT NULL DEFAULT '{}' CHECK (json_typeof(rest_settings::json)='object')
);

-- Sign-in and recovery look accounts up by address, ignoring case.
CREATE INDEX idx_users_email_lower ON users(lower(email));

CREATE INDEX IF NOT EXISTS idx_users_avatar_file ON users(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_email_outbox_due ON email_outbox(next_attempt_at) WHERE sent_at IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_signup_requests_username_pending
ON signup_requests (username_key)
WHERE status = 'pending';

CREATE UNIQUE INDEX IF NOT EXISTS idx_signup_requests_email_pending
ON signup_requests (email_key)
WHERE email IS NOT NULL AND status = 'pending';

CREATE INDEX IF NOT EXISTS idx_signup_requests_status_created_at
ON signup_requests (status, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_signup_requests_processed_hash ON signup_requests(id) WHERE status <> 'pending' AND password_hash <> '';

CREATE INDEX IF NOT EXISTS idx_organizations_avatar_file ON organizations(avatar_file_id) WHERE avatar_file_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_repositories_storage_node ON repositories(storage_node_id, id);

CREATE INDEX IF NOT EXISTS idx_repository_pushes_repo ON repository_pushes(repo_id, created_at);

CREATE INDEX IF NOT EXISTS idx_repository_pushes_created_at ON repository_pushes(created_at);

CREATE INDEX IF NOT EXISTS idx_issue_assignees_user ON issue_assignees(user_id);

CREATE INDEX IF NOT EXISTS idx_pull_requests_merged_by ON pull_requests(merged_by_id, merged_at) WHERE merged_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_issue_labels_label ON issue_labels(label_id);

CREATE INDEX IF NOT EXISTS idx_pr_labels_label ON pr_labels(label_id);

CREATE INDEX IF NOT EXISTS idx_pr_assignees_user ON pr_assignees(user_id);

CREATE INDEX IF NOT EXISTS idx_pr_reviewers_user ON pr_reviewers(user_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_pull_request_viewed_files_path
    ON pull_request_viewed_files (user_id, pull_request_id, md5(path));

CREATE INDEX IF NOT EXISTS idx_release_assets_file_id ON release_assets(file_id);

CREATE INDEX IF NOT EXISTS idx_upload_rate_limit_user_reason_time ON upload_rate_limit(user_id, reason, uploaded_at);

CREATE INDEX IF NOT EXISTS idx_notifications_repo_id ON notifications(repo_id);

CREATE INDEX IF NOT EXISTS idx_ci_runs_repo_head ON ci_runs(repo_id, head_sha, id);

CREATE INDEX IF NOT EXISTS idx_ci_runs_pull_request ON ci_runs(pull_request_id) WHERE pull_request_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_ci_runs_push_created_at ON ci_runs(created_at) WHERE event = 'push' AND triggered_by_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_ci_deployments_run ON ci_deployments(run_id);

CREATE INDEX IF NOT EXISTS idx_commit_statuses_ci_run_id ON commit_statuses(ci_run_id) WHERE ci_run_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_auth_login_limits_expiry ON auth_login_limits (window_ends_at);

CREATE TABLE repository_star_days (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day DATE NOT NULL,
    stars INTEGER NOT NULL CHECK (stars >= 0),
    PRIMARY KEY (repo_id, day)
);
CREATE FUNCTION record_repository_star_day() RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO repository_star_days (repo_id,day,stars) VALUES (NEW.repo_id,(NEW.created_at AT TIME ZONE 'UTC')::date,1)
    ON CONFLICT(repo_id,day) DO UPDATE SET stars=repository_star_days.stars+1;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER record_repository_star_day AFTER INSERT ON stars FOR EACH ROW EXECUTE FUNCTION record_repository_star_day();

CREATE TABLE pull_request_stacks (
    id BIGINT PRIMARY KEY REFERENCES pull_requests(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    base_branch VARCHAR(255) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(repo_id,number)
);

CREATE TABLE user_social_accounts (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK (length(provider) BETWEEN 1 AND 50),
    url TEXT NOT NULL CHECK (length(url) BETWEEN 1 AND 255),
    PRIMARY KEY (user_id, url)
);

CREATE TABLE user_profile_settings (
    user_id BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    company TEXT CHECK (length(company) <= 255),
    twitter_username TEXT CHECK (length(twitter_username) <= 255),
    hireable BOOLEAN,
    public_email TEXT CHECK (length(public_email) <= 255),
    primary_visibility TEXT NOT NULL DEFAULT 'private' CHECK (primary_visibility IN ('private', 'public'))
);

CREATE TABLE ssh_signing_keys (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 100),
    key TEXT NOT NULL CHECK (length(key) <= 32768),
    fingerprint TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_ssh_signing_keys_user ON ssh_signing_keys(user_id, id);

CREATE TABLE user_emails (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email TEXT NOT NULL CHECK (length(email) BETWEEN 1 AND 255),
    verified BOOLEAN NOT NULL DEFAULT FALSE,
    verification_hash TEXT UNIQUE CHECK (length(verification_hash)=64),
    verification_expires_at TIMESTAMPTZ,
    last_sent_at TIMESTAMPTZ,
    PRIMARY KEY (user_id, email)
);
CREATE UNIQUE INDEX idx_user_emails_address ON user_emails(lower(email));

-- Primary and secondary enrollment must agree on an address's owner.
CREATE FUNCTION guard_primary_email() RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(hashtext(current_schema()),37004);
    IF EXISTS (SELECT 1 FROM user_emails WHERE lower(email)=lower(NEW.email) AND user_id<>NEW.id) THEN
        RAISE EXCEPTION 'Email address is already registered' USING ERRCODE='23505';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER users_primary_email BEFORE INSERT OR UPDATE OF email ON users FOR EACH ROW EXECUTE FUNCTION guard_primary_email();

CREATE TABLE user_blocks (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    blocked_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id,blocked_id),
    CHECK (user_id<>blocked_id)
);
CREATE INDEX idx_user_blocks_target ON user_blocks(blocked_id,user_id);

CREATE TABLE gpg_keys (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    primary_key_id BIGINT REFERENCES gpg_keys(id) ON DELETE CASCADE,
    name TEXT CHECK (length(name)<=100),
    fingerprint TEXT NOT NULL UNIQUE,
    key_id TEXT NOT NULL,
    key_data TEXT NOT NULL CHECK (length(key_data)<=262144)
);
CREATE INDEX idx_gpg_keys_user ON gpg_keys(user_id,primary_key_id,id);
CREATE INDEX idx_gpg_keys_key_id ON gpg_keys(key_id);

CREATE TABLE interaction_limits (
    user_id BIGINT UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    org_id BIGINT UNIQUE REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT UNIQUE REFERENCES repositories(id) ON DELETE CASCADE,
    restriction TEXT NOT NULL CHECK (restriction IN ('existing_users','contributors_only','collaborators_only')),
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK ((user_id IS NOT NULL)::integer+(org_id IS NOT NULL)::integer+(repo_id IS NOT NULL)::integer=1)
);

CREATE TABLE interaction_contributors (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    write_version BIGINT NOT NULL,
    default_branch TEXT NOT NULL,
    storage_node_id BIGINT,
    emails_hash TEXT NOT NULL,
    contributed BOOLEAN NOT NULL,
    PRIMARY KEY(repo_id,user_id)
);

CREATE TABLE interaction_pull_caps (
    org_id BIGINT UNIQUE REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT UNIQUE REFERENCES repositories(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    max_open_pull_requests INTEGER NOT NULL DEFAULT 1 CHECK (max_open_pull_requests BETWEEN 1 AND 1000),
    include_drafts BOOLEAN NOT NULL DEFAULT FALSE,
    CHECK ((org_id IS NOT NULL)::integer+(repo_id IS NOT NULL)::integer=1)
);

CREATE TABLE interaction_pull_bypass (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY(repo_id,user_id)
);
CREATE INDEX idx_pull_cap_author ON pull_requests(author_id,state,repo_id,is_draft);

-- Retain issued names so deleted gist URLs cannot resolve to legacy repositories.
CREATE TABLE gist_identifiers (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id)=32)
);

CREATE TABLE gists (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    id TEXT NOT NULL UNIQUE REFERENCES gist_identifiers(id) CHECK (length(id)=32),
    is_public BOOLEAN NOT NULL DEFAULT FALSE,
    description TEXT NOT NULL DEFAULT '' CHECK (length(description)<=65535),
    ready BOOLEAN NOT NULL DEFAULT FALSE,
    pending_sha TEXT,
    pending_base TEXT,
    pending_description TEXT,
    pending_storage_node_id BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK ((pending_sha IS NULL)=(pending_description IS NULL))
);
CREATE INDEX idx_gists_updated ON gists(updated_at DESC,id DESC);

CREATE TABLE gist_stars (
    repo_id BIGINT NOT NULL REFERENCES gists(repo_id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(repo_id,user_id)
);
CREATE INDEX idx_gist_stars_user ON gist_stars(user_id,repo_id);

CREATE TABLE gist_comments (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES gists(repo_id) ON DELETE CASCADE,
    user_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65535),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_gist_comments_repo ON gist_comments(repo_id,id);

CREATE TABLE issue_comment_pins (
    issue_id BIGINT PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE,
    comment_id BIGINT NOT NULL UNIQUE REFERENCES issue_comments(id) ON DELETE CASCADE,
    pinned_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    pinned_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE issue_attribute_guard (id BIGINT PRIMARY KEY CHECK(id=1));
INSERT INTO issue_attribute_guard VALUES(1);

CREATE TABLE issue_attributes (
    id BIGSERIAL PRIMARY KEY,
    owner_user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('type','field')),
    name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 128),
    description TEXT,
    data_type TEXT CHECK(data_type IN ('text','number','date','single_select','multi_select')),
    visibility TEXT NOT NULL DEFAULT 'all' CHECK(visibility IN ('all','organization_members_only')),
    is_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    color TEXT CHECK(color IN ('gray','blue','green','yellow','orange','red','pink','purple')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK((owner_user_id IS NOT NULL)::int+(owner_org_id IS NOT NULL)::int=1),
    CHECK((kind='type' AND data_type IS NULL) OR (kind='field' AND data_type IS NOT NULL AND owner_org_id IS NOT NULL))
);
CREATE UNIQUE INDEX issue_attribute_user_name ON issue_attributes(owner_user_id,kind,lower(name)) WHERE owner_user_id IS NOT NULL;
CREATE UNIQUE INDEX issue_attribute_org_name ON issue_attributes(owner_org_id,kind,lower(name)) WHERE owner_org_id IS NOT NULL;

CREATE TABLE issue_attribute_options (
    id BIGSERIAL PRIMARY KEY,
    attribute_id BIGINT NOT NULL REFERENCES issue_attributes(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    color TEXT NOT NULL CHECK(color IN ('gray','blue','green','yellow','orange','red','pink','purple')),
    priority BIGINT NOT NULL,
    description TEXT CHECK (length(description)<=1024)
);
CREATE UNIQUE INDEX issue_attribute_option_name ON issue_attribute_options(attribute_id,lower(name));

CREATE TABLE issue_type_assignments (
    issue_id BIGINT PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE,
    attribute_id BIGINT NOT NULL REFERENCES issue_attributes(id) ON DELETE CASCADE
);

CREATE TABLE issue_field_values (
    id BIGSERIAL PRIMARY KEY,
    issue_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    attribute_id BIGINT NOT NULL REFERENCES issue_attributes(id) ON DELETE CASCADE,
    value_json TEXT NOT NULL CHECK(value_json::jsonb IS NOT NULL),
    UNIQUE(issue_id,attribute_id)
);

CREATE TABLE issue_relation_guard (id BIGINT PRIMARY KEY CHECK (id=1));
INSERT INTO issue_relation_guard VALUES (1);

CREATE TABLE issue_relations (
    kind TEXT NOT NULL CHECK (kind IN ('parent','blocks','related')),
    source_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    target_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    position BIGINT NOT NULL DEFAULT 0 CHECK (position>=0),
    PRIMARY KEY(kind,source_id,target_id),
    CHECK (source_id<>target_id),
    CHECK (kind<>'related' OR source_id<target_id)
);
CREATE UNIQUE INDEX issue_one_parent ON issue_relations(target_id) WHERE kind='parent';
CREATE INDEX issue_relations_target ON issue_relations(kind,target_id);

CREATE TABLE pr_comment_pins (
    pull_request_id BIGINT PRIMARY KEY REFERENCES pull_requests(id) ON DELETE CASCADE,
    comment_id BIGINT NOT NULL UNIQUE REFERENCES pr_comments(id) ON DELETE CASCADE,
    pinned_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    pinned_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- issues: reviewable agent proposals
CREATE TABLE repository_issue_suggestions (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE issue_suggestions (
    id BIGSERIAL PRIMARY KEY,
    issue_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    action TEXT NOT NULL CHECK(action IN ('set_type','add_label','add_field','add_assignee','close_issue')),
    slot TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','applied','approved','dismissed','replaced','invalidated')),
    target_id BIGINT CHECK(target_id>0),
    target_name TEXT NOT NULL,
    value_json TEXT NOT NULL CHECK(value_json::jsonb IS NOT NULL),
    rationale TEXT,
    confidence TEXT CHECK(confidence IN ('LOW','MEDIUM','HIGH')),
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    issue_event_id BIGINT REFERENCES timeline_events(id) ON DELETE SET NULL,
    resolved_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX issue_suggestions_state ON issue_suggestions(issue_id,state,id);
CREATE INDEX issue_suggestions_history ON issue_suggestions(issue_id,created_at,id);
CREATE INDEX issue_suggestions_actor ON issue_suggestions(issue_id,actor_id,created_at,id);
CREATE INDEX issue_suggestions_target ON issue_suggestions(action,target_id) WHERE state='pending';
CREATE UNIQUE INDEX issue_suggestions_current_actor_slot ON issue_suggestions(issue_id,actor_id,slot) WHERE state='pending';
CREATE FUNCTION issue_suggestion_target_deleted() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_ARGV[0]='attribute' THEN
        UPDATE issue_suggestions SET state='invalidated',updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE target_id=OLD.id AND state='pending' AND action=CASE WHEN OLD.kind='type' THEN 'set_type' ELSE 'add_field' END;
    ELSE
        UPDATE issue_suggestions SET state='invalidated',updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE target_id=OLD.id AND state='pending' AND action=TG_ARGV[0];
    END IF;
    RETURN OLD;
END;
$$;
CREATE TRIGGER issue_suggestion_attribute_deleted AFTER DELETE ON issue_attributes FOR EACH ROW EXECUTE FUNCTION issue_suggestion_target_deleted('attribute');
CREATE TRIGGER issue_suggestion_label_deleted AFTER DELETE ON labels FOR EACH ROW EXECUTE FUNCTION issue_suggestion_target_deleted('add_label');
CREATE TRIGGER issue_suggestion_assignee_deleted AFTER DELETE ON users FOR EACH ROW EXECUTE FUNCTION issue_suggestion_target_deleted('add_assignee');
CREATE TRIGGER issue_suggestion_duplicate_deleted AFTER DELETE ON issues FOR EACH ROW EXECUTE FUNCTION issue_suggestion_target_deleted('close_issue');
CREATE FUNCTION issue_suggestion_definition_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE issue_suggestions SET state='invalidated',updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE target_id=NEW.id AND state='pending' AND ((NEW.kind='type' AND OLD.is_enabled AND NOT NEW.is_enabled AND action='set_type') OR (NEW.kind='field' AND action='add_field'));
    RETURN NEW;
END;
$$;
CREATE TRIGGER issue_suggestion_definition_changed AFTER UPDATE ON issue_attributes FOR EACH ROW EXECUTE FUNCTION issue_suggestion_definition_changed();

-- pulls
CREATE TABLE events (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    is_public BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX events_created ON events(created_at DESC,id DESC);
CREATE INDEX events_repository ON events(repo_id,created_at DESC,id DESC);
CREATE INDEX events_actor ON events(actor_id,created_at DESC,id DESC);

-- pulls
CREATE TABLE activity_feed_tokens (
    api_token_id BIGINT PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    value TEXT NOT NULL UNIQUE
);

-- pulls
CREATE TABLE pull_request_async_merges (
    uuid TEXT PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    token_id BIGINT REFERENCES personal_access_tokens(id) ON DELETE SET NULL,
    worker_node_id BIGINT REFERENCES nodes(id) ON DELETE SET NULL,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    number INTEGER NOT NULL,
    request JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','merged','failed')),
    details JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (NOW()+INTERVAL '1 day')
);
CREATE UNIQUE INDEX pull_request_async_pending ON pull_request_async_merges(pull_request_id) WHERE status='pending';
CREATE INDEX pull_request_async_expiry ON pull_request_async_merges(expires_at);

CREATE TABLE notification_thread_subscriptions (
    thread_id BIGINT PRIMARY KEY REFERENCES notification_threads(id) ON DELETE CASCADE,
    is_explicit BOOLEAN NOT NULL DEFAULT FALSE,
    subscribed BOOLEAN NOT NULL DEFAULT TRUE,
    ignored BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX notifications_thread ON notifications(thread_id,id DESC);
CREATE FUNCTION notification_threads_provenance() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.user_id IS DISTINCT FROM OLD.user_id OR NEW.scope_repository_id IS DISTINCT FROM OLD.scope_repository_id OR NEW.subject_type IS DISTINCT FROM OLD.subject_type OR NEW.subject_key IS DISTINCT FROM OLD.subject_key THEN
        RAISE EXCEPTION 'notification thread provenance is immutable';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER notification_threads_provenance BEFORE UPDATE OF user_id,scope_repository_id,subject_type,subject_key ON notification_threads FOR EACH ROW EXECUTE FUNCTION notification_threads_provenance();
CREATE FUNCTION notifications_thread_scope() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.thread_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM notification_threads t WHERE t.id=NEW.thread_id AND t.user_id=NEW.user_id AND t.scope_repository_id=NEW.scope_repository_id) THEN
        RAISE EXCEPTION 'notification thread scope mismatch';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER notifications_thread_scope BEFORE INSERT OR UPDATE OF thread_id ON notifications FOR EACH ROW EXECUTE FUNCTION notifications_thread_scope();
CREATE UNIQUE INDEX pull_requests_source_issue ON pull_requests(source_issue_id) WHERE source_issue_id IS NOT NULL;
CREATE UNIQUE INDEX pr_comments_source_issue ON pr_comments(source_issue_comment_id) WHERE source_issue_comment_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS actions_runtime_tokens (
    run_id BIGINT PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    backend_id TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS actions_objects (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    run_id BIGINT REFERENCES ci_runs(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (kind IN ('cache','artifact')),
    scope TEXT NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 512),
    version TEXT NOT NULL,
    size BIGINT NOT NULL DEFAULT 0 CHECK (size >= 0),
    finalized BOOLEAN NOT NULL DEFAULT FALSE,
    manifest TEXT,
    created_at BIGINT NOT NULL,
    accessed_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS actions_cache_key ON actions_objects(repo_id,scope,name,version) WHERE kind='cache';
CREATE UNIQUE INDEX IF NOT EXISTS actions_artifact_name ON actions_objects(run_id,name) WHERE kind='artifact';
CREATE INDEX IF NOT EXISTS actions_objects_expiry ON actions_objects(expires_at);
CREATE INDEX IF NOT EXISTS actions_objects_run ON actions_objects(run_id);
CREATE OR REPLACE FUNCTION actions_run_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM actions_objects WHERE run_id=OLD.id AND (kind='artifact' OR NOT finalized);
    RETURN OLD;
END;
$$;
CREATE TRIGGER actions_run_delete BEFORE DELETE ON ci_runs
    FOR EACH ROW EXECUTE FUNCTION actions_run_delete();

CREATE TABLE IF NOT EXISTS actions_blob_parts (
    object_id BIGINT NOT NULL REFERENCES actions_objects(id) ON DELETE CASCADE,
    block_id TEXT NOT NULL,
    object_key TEXT NOT NULL UNIQUE,
    size BIGINT NOT NULL CHECK (size >= 0),
    ready BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY(object_id,block_id)
);
CREATE OR REPLACE FUNCTION actions_blob_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO pending_object_deletions(object_key,delete_after)
    VALUES(OLD.object_key,NOW()+INTERVAL '1 hour') ON CONFLICT(object_key) DO NOTHING;
    RETURN OLD;
END;
$$;
CREATE TRIGGER actions_blob_delete AFTER DELETE ON actions_blob_parts
    FOR EACH ROW EXECUTE FUNCTION actions_blob_delete();

CREATE TABLE IF NOT EXISTS actions_blob_grants (
    token_hash TEXT PRIMARY KEY,
    object_id BIGINT NOT NULL REFERENCES actions_objects(id) ON DELETE CASCADE,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    write BOOLEAN NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS actions_blob_grants_expiry ON actions_blob_grants(expires_at);

CREATE TABLE actions_github_tokens (
    token_id BIGINT PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    job TEXT NOT NULL,
    permissions TEXT NOT NULL,
    revoked_at BIGINT
);
CREATE INDEX actions_github_tokens_run ON actions_github_tokens(run_id);
CREATE FUNCTION actions_bot_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (OLD.username='github-actions[bot]' AND (NEW.username<>'github-actions[bot]' OR NEW.is_admin OR NEW.password_hash IS NOT NULL OR NEW.email IS NOT NULL)) OR (OLD.username<>'github-actions[bot]' AND NEW.username='github-actions[bot]') THEN
        RAISE EXCEPTION 'Actions bot identity is reserved';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER actions_bot_identity BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION actions_bot_identity();
CREATE FUNCTION actions_bot_session() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM users WHERE id=NEW.user_id AND username='github-actions[bot]') THEN
        RAISE EXCEPTION 'Actions bot cannot start browser sessions';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER actions_bot_session BEFORE INSERT ON sessions
    FOR EACH ROW EXECUTE FUNCTION actions_bot_session();
CREATE FUNCTION actions_github_tokens_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM actions_github_tokens WHERE run_id=OLD.id);
    RETURN OLD;
END;
$$;
CREATE TRIGGER actions_github_tokens_delete BEFORE DELETE ON ci_runs
    FOR EACH ROW EXECUTE FUNCTION actions_github_tokens_delete();

-- actions-runs
CREATE TABLE org_actions_settings (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    cache_size_gib INTEGER CHECK (cache_size_gib IS NULL OR cache_size_gib>0),
    cache_retention_days INTEGER CHECK (cache_retention_days IS NULL OR cache_retention_days>0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    rest_settings TEXT NOT NULL DEFAULT '{}' CHECK (json_typeof(rest_settings::json)='object')
);

CREATE TABLE actions_workflows (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','disabled_manually')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    next_run_number BIGINT NOT NULL DEFAULT 0,
    UNIQUE(repo_id,path)
);

-- graphql
CREATE TABLE graphql_rate_limits (
    token_id BIGINT PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    used INTEGER NOT NULL CHECK (used BETWEEN 1 AND 5000),
    reset_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX ci_runs_workflow_run ON ci_runs(workflow_run_id,workflow_run_attempt);

CREATE TABLE actions_run_metadata (
    run_id BIGINT PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    workflow_id BIGINT NOT NULL REFERENCES actions_workflows(id),
    head_repository JSONB,
    head_owner TEXT
);
CREATE INDEX actions_jobs_run ON actions_jobs(run_id,id);

CREATE TABLE actions_steps (
    job_id BIGINT NOT NULL REFERENCES actions_jobs(id) ON DELETE CASCADE,
    number INTEGER NOT NULL CHECK(number>0),
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('in_progress','completed')),
    conclusion TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ,
    duration_ms BIGINT CHECK(duration_ms>=0),
    PRIMARY KEY(job_id,number)
);

CREATE TABLE actions_job_states (
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    generation TEXT NOT NULL,
    workflow_path TEXT NOT NULL,
    state JSONB NOT NULL,
    PRIMARY KEY(run_id,generation,workflow_path)
);
CREATE INDEX ci_logs_job ON ci_logs(job_id,id);

CREATE TABLE actions_log_downloads (
    token_hash TEXT PRIMARY KEY,
    resource TEXT NOT NULL,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    job_id BIGINT REFERENCES actions_jobs(id) ON DELETE CASCADE,
    step_number INTEGER,
    log_version BIGINT NOT NULL,
    max_log_id BIGINT NOT NULL,
    manifest JSONB NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE INDEX actions_log_downloads_expiry ON actions_log_downloads(expires_at);

CREATE TABLE actions_replays (
    run_id BIGINT PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    source_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    data JSONB,
    debug BOOLEAN NOT NULL DEFAULT FALSE
);

-- git
CREATE TABLE commit_status_history (
    id BIGSERIAL PRIMARY KEY,
    status_id BIGINT NOT NULL REFERENCES commit_statuses(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    context TEXT NOT NULL,
    state TEXT NOT NULL,
    description TEXT,
    target_url TEXT,
    creator_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX idx_commit_status_history_page ON commit_status_history(repo_id, sha, id DESC);
CREATE INDEX idx_commit_status_history_latest ON commit_status_history(status_id, id DESC);
CREATE INDEX idx_commit_status_history_context ON commit_status_history(repo_id, sha, context, id DESC);
CREATE FUNCTION record_commit_status_history() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO commit_status_history(status_id,repo_id,sha,context,state,description,target_url,creator_id,created_at)
        VALUES(NEW.id,NEW.repo_id,NEW.sha,NEW.context,NEW.state,NEW.description,NEW.target_url,NEW.creator_id,clock_timestamp());
    RETURN NEW;
END;
$$;
CREATE TRIGGER commit_status_history_insert AFTER INSERT ON commit_statuses FOR EACH ROW EXECUTE FUNCTION record_commit_status_history();
CREATE TRIGGER commit_status_history_update AFTER UPDATE OF state,description,target_url,creator_id,updated_at ON commit_statuses FOR EACH ROW EXECUTE FUNCTION record_commit_status_history();
CREATE VIEW commit_statuses_latest AS
    SELECT h.id,h.repo_id,h.sha,h.context,h.state,h.description,h.target_url,h.creator_id,h.created_at,h.created_at AS updated_at
    FROM commit_status_history h JOIN commit_statuses s ON s.id=h.status_id
    WHERE h.id=(SELECT MAX(id) FROM commit_status_history WHERE status_id=s.id);

CREATE TABLE commit_comments (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    author_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    sha TEXT NOT NULL CHECK (sha ~ '^[0-9a-f]{40}$'),
    body TEXT NOT NULL CHECK (char_length(body) BETWEEN 1 AND 65536),
    path TEXT CHECK (path IS NULL OR octet_length(path) BETWEEN 1 AND 4096),
    position INTEGER CHECK (position IS NULL OR position>0),
    line INTEGER CHECK (line IS NULL OR line>0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (path IS NOT NULL OR (position IS NULL AND line IS NULL))
);
CREATE INDEX commit_comments_repository ON commit_comments(repo_id,id);
CREATE INDEX commit_comments_commit ON commit_comments(repo_id,sha,id);
CREATE FUNCTION delete_commit_comment_reactions() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN
    DELETE FROM reactions WHERE subject_kind='commit_comment' AND subject_id=OLD.id;
    RETURN OLD;
END $$;
CREATE TRIGGER commit_comment_reactions AFTER DELETE ON commit_comments FOR EACH ROW EXECUTE FUNCTION delete_commit_comment_reactions();

CREATE INDEX pull_requests_commit_associations ON pull_requests(repo_id,id DESC) WHERE state='open' OR merged;
CREATE FUNCTION touch_milestone_updated_at() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.updated_at=clock_timestamp(); RETURN NEW; END;
$$;
CREATE TRIGGER milestones_updated BEFORE UPDATE ON milestones FOR EACH ROW EXECUTE FUNCTION touch_milestone_updated_at();

-- Catalog timestamps belong to the catalog record, not an invented historical first use.
CREATE TABLE topics (
    name TEXT PRIMARY KEY CHECK (name ~ '^[a-z0-9-]{1,35}$'),
    display_name TEXT, short_description TEXT, description TEXT, created_by TEXT, released TEXT,
    featured BOOLEAN NOT NULL DEFAULT FALSE, curated BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE FUNCTION register_repository_topic() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN
    INSERT INTO topics(name) VALUES (NEW.topic) ON CONFLICT DO NOTHING;
    RETURN NEW;
END $$;
CREATE TRIGGER repository_topic_catalog AFTER INSERT ON repository_topics FOR EACH ROW EXECUTE FUNCTION register_repository_topic();
CREATE FUNCTION touch_label_timestamp() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN
    IF (OLD.name,OLD.color,OLD.description) IS DISTINCT FROM (NEW.name,NEW.color,NEW.description) THEN NEW.updated_at=clock_timestamp(); END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER label_timestamp BEFORE UPDATE OF name,color,description ON labels FOR EACH ROW EXECUTE FUNCTION touch_label_timestamp();

CREATE TABLE team_invitations (
    team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    invitation_id BIGINT NOT NULL REFERENCES organization_invitations(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('member','maintainer')),
    PRIMARY KEY (team_id, invitation_id)
);
CREATE INDEX team_invitations_invitation ON team_invitations(invitation_id);
CREATE INDEX teams_parent ON teams(parent_team_id,id);

CREATE VIEW team_ancestors AS
WITH RECURSIVE ancestors(team_id,ancestor_id) AS (
    SELECT id,id FROM teams WHERE privacy IN ('visible','secret')
    UNION
    SELECT a.team_id,p.id FROM ancestors a JOIN teams t ON t.id=a.ancestor_id
    JOIN teams p ON p.id=t.parent_team_id AND p.org_id=t.org_id
    WHERE t.privacy='visible' AND p.privacy='visible'
) SELECT team_id,ancestor_id FROM ancestors;

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
    id BIGSERIAL PRIMARY KEY,
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
    id BIGSERIAL PRIMARY KEY,
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
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK((org_id IS NOT NULL AND repo_id IS NULL) OR (org_id IS NULL AND repo_id IS NOT NULL))
);

CREATE TABLE repo_actions_keys (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BYTEA NOT NULL CHECK (octet_length(encrypted_key)=48),
    nonce BYTEA NOT NULL CHECK (octet_length(nonce)=12)
);

CREATE TABLE repo_actions_variables (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    value TEXT NOT NULL CHECK (octet_length(value)<=49152),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(repo_id,name)
);

-- apps
CREATE TABLE oauth_applications (
    id BIGSERIAL PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL UNIQUE,
    secret_hash TEXT NOT NULL,
    name TEXT NOT NULL,
    homepage TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    is_github_app BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE oauth_app_grants (
    hash TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    user_code TEXT NOT NULL UNIQUE,
    scopes TEXT NOT NULL,
    expires_at BIGINT NOT NULL,
    interval BIGINT NOT NULL DEFAULT 5,
    last_poll BIGINT NOT NULL DEFAULT 0,
    user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    session_id TEXT,
    denied BOOLEAN NOT NULL DEFAULT FALSE,
    kind TEXT NOT NULL CHECK (kind IN ('device','web')),
    redirect_uri TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT '',
    challenge TEXT NOT NULL DEFAULT ''
);
CREATE INDEX oauth_app_grants_expiry ON oauth_app_grants(expires_at);

CREATE TABLE oauth_app_tokens (
    token_id BIGINT PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL,
    scopes TEXT NOT NULL
);
CREATE INDEX oauth_app_tokens_client ON oauth_app_tokens(client_id);

-- apps: GitHub Apps and installations
CREATE TABLE github_apps (
    id BIGSERIAL PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    bot_id BIGINT NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL UNIQUE REFERENCES oauth_applications(client_id) ON DELETE CASCADE,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    homepage TEXT NOT NULL,
    public_key TEXT NOT NULL,
    permissions TEXT NOT NULL,
    events TEXT NOT NULL,
    access_level SMALLINT NOT NULL CHECK (access_level IN (1,3,5)),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    is_public BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE github_app_installations (
    id BIGSERIAL PRIMARY KEY,
    app_id BIGINT NOT NULL REFERENCES github_apps(id) ON DELETE CASCADE,
    user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    all_repos BOOLEAN NOT NULL,
    suspended_at TIMESTAMPTZ,
    suspended_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK ((user_id IS NULL) <> (org_id IS NULL)),
    UNIQUE (app_id,user_id), UNIQUE (app_id,org_id)
);

CREATE TABLE github_app_repositories (
    installation_id BIGINT NOT NULL REFERENCES github_app_installations(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(installation_id,repo_id)
);

CREATE TABLE github_app_tokens (
    token_id BIGINT PRIMARY KEY REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    -- Retain credential bounds after an account/organization cascade removes its installation.
    -- Dropping this marker would turn a scoped user token back into an unscoped OAuth token.
    installation_id BIGINT NOT NULL,
    permissions TEXT NOT NULL,
    repositories TEXT NOT NULL,
    user_token BOOLEAN NOT NULL
);
CREATE INDEX github_app_tokens_installation ON github_app_tokens(installation_id);

-- apps: account installation requests
CREATE TABLE github_app_requests (
    id BIGSERIAL PRIMARY KEY,
    app_id BIGINT NOT NULL REFERENCES github_apps(id) ON DELETE CASCADE,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(app_id,org_id,user_id)
);

-- apps: webhook configuration and recorded attempts
CREATE TABLE github_app_hooks (
    app_id BIGINT PRIMARY KEY REFERENCES github_apps(id) ON DELETE CASCADE,
    payload_url TEXT NOT NULL CHECK(length(payload_url)<=2048),
    content_type TEXT NOT NULL CHECK(content_type IN ('application/json','application/x-www-form-urlencoded')),
    encrypted_secret BYTEA,
    secret_nonce BYTEA,
    events TEXT NOT NULL,
    active BOOLEAN NOT NULL,
    ssl_verify BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK((encrypted_secret IS NULL)=(secret_nonce IS NULL))
);

CREATE TABLE github_app_deliveries (
    id BIGSERIAL PRIMARY KEY,
    webhook_id BIGINT NOT NULL REFERENCES github_app_hooks(app_id) ON DELETE CASCADE,
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
    redelivery BOOLEAN NOT NULL,
    installation_id BIGINT,
    repository_id BIGINT,
    url TEXT NOT NULL,
    delivered_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX github_app_deliveries_hook ON github_app_deliveries(webhook_id,id);

CREATE TABLE github_app_callbacks (
    app_id BIGINT NOT NULL REFERENCES github_apps(id) ON DELETE CASCADE,
    uri TEXT NOT NULL,
    PRIMARY KEY(app_id,uri)
);

CREATE TABLE github_app_manifests (
    hash TEXT PRIMARY KEY,
    conversion_hash TEXT UNIQUE,
    manifest TEXT NOT NULL,
    state TEXT NOT NULL,
    user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    expires_at BIGINT NOT NULL,
    CHECK((user_id IS NULL)=(session_id IS NULL))
);
CREATE INDEX github_app_manifests_expiry ON github_app_manifests(expires_at);

-- apps: installation identities cannot become interactive accounts
CREATE FUNCTION github_app_bot_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM github_apps WHERE bot_id=OLD.id) AND (NEW.username<>OLD.username OR NEW.is_admin OR NEW.password_hash IS NOT NULL OR NEW.email IS NOT NULL) THEN
        RAISE EXCEPTION 'GitHub App bot identity is reserved';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER github_app_bot_identity BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION github_app_bot_identity();
CREATE FUNCTION github_app_bot_session() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM github_apps WHERE bot_id=NEW.user_id) THEN
        RAISE EXCEPTION 'GitHub App bots cannot start browser sessions';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER github_app_bot_session BEFORE INSERT OR UPDATE OF user_id ON sessions FOR EACH ROW EXECUTE FUNCTION github_app_bot_session();

-- deploy
CREATE TABLE deployment_environments (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL,
    wait_timer BIGINT NOT NULL DEFAULT 0 CHECK (wait_timer BETWEEN 0 AND 43200),
    prevent_self_review BOOLEAN NOT NULL DEFAULT false,
    reviewers TEXT NOT NULL DEFAULT '[]' CHECK (json_typeof(reviewers::json)='array' AND json_array_length(reviewers::json)<=6),
    branch_policy TEXT CHECK (branch_policy IN ('protected','custom')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(repo_id,name_key)
);

CREATE TABLE deployment_branch_policies (
    id BIGSERIAL PRIMARY KEY,
    environment_id BIGINT NOT NULL REFERENCES deployment_environments(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('branch','tag')),
    UNIQUE(environment_id,name,kind)
);

CREATE TABLE repository_deployments (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    creator_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    sha TEXT NOT NULL,
    reference TEXT NOT NULL,
    task TEXT NOT NULL DEFAULT 'deploy',
    payload TEXT NOT NULL DEFAULT '{}',
    original_environment TEXT NOT NULL,
    environment TEXT NOT NULL,
    description TEXT,
    transient_environment BOOLEAN NOT NULL DEFAULT false,
    production_environment BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX repository_deployments_repo ON repository_deployments(repo_id,id);

CREATE TABLE deployment_statuses (
    id BIGSERIAL PRIMARY KEY,
    deployment_id BIGINT NOT NULL REFERENCES repository_deployments(id) ON DELETE CASCADE,
    creator_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('error','failure','inactive','in_progress','queued','pending','success')),
    description TEXT NOT NULL DEFAULT '' CHECK (length(description)<=140),
    environment TEXT NOT NULL,
    log_url TEXT NOT NULL DEFAULT '',
    environment_url TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX deployment_statuses_deployment ON deployment_statuses(deployment_id,id);

-- packages
CREATE TABLE packages (
    id BIGSERIAL PRIMARY KEY,
    owner_kind TEXT NOT NULL CHECK (owner_kind IN ('user','org')),
    owner_user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    package_type TEXT NOT NULL CHECK (package_type IN ('container','npm','maven','nuget','rubygems')),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('public','private')),
    repo_id BIGINT REFERENCES repositories(id) ON DELETE SET NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    deleted_at BIGINT,
    CHECK ((owner_kind='user' AND owner_user_id IS NOT NULL AND owner_org_id IS NULL) OR (owner_kind='org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL)),
    UNIQUE(owner_user_id,package_type,name),
    UNIQUE(owner_org_id,package_type,name)
);

CREATE TABLE package_versions (
    id BIGSERIAL PRIMARY KEY,
    package_id BIGINT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    digest TEXT NOT NULL,
    media_type TEXT NOT NULL,
    manifest TEXT NOT NULL,
    downloads BIGINT NOT NULL DEFAULT 0 CHECK (downloads>=0),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    deleted_at BIGINT,
    UNIQUE(package_id,name)
);

CREATE TABLE package_tags (
    package_id BIGINT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    version_id BIGINT NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
    PRIMARY KEY(package_id,tag)
);
CREATE INDEX package_tags_version ON package_tags(version_id);

CREATE TABLE package_blobs (
    digest TEXT PRIMARY KEY,
    size BIGINT NOT NULL CHECK (size>=0),
    ready BOOLEAN NOT NULL DEFAULT false,
    deleting BOOLEAN NOT NULL DEFAULT false,
    expires_at BIGINT NOT NULL
);

CREATE TABLE package_blob_grants (
    package_id BIGINT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    digest TEXT NOT NULL REFERENCES package_blobs(digest) ON DELETE CASCADE,
    expires_at BIGINT NOT NULL,
    PRIMARY KEY(package_id,digest)
);

CREATE TABLE package_version_blobs (
    version_id BIGINT NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
    digest TEXT NOT NULL REFERENCES package_blobs(digest),
    PRIMARY KEY(version_id,digest)
);
CREATE INDEX package_version_blobs_digest ON package_version_blobs(digest);

CREATE TABLE package_files (
    package_id BIGINT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    version_id BIGINT NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
    digest TEXT NOT NULL REFERENCES package_blobs(digest),
    PRIMARY KEY(package_id,path)
);
CREATE INDEX package_files_path ON package_files(path,package_id);
CREATE INDEX package_files_version ON package_files(version_id);

CREATE TABLE package_uploads (
    id TEXT PRIMARY KEY,
    package_id BIGINT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    token_id BIGINT NOT NULL REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    uploaded_bytes BIGINT NOT NULL DEFAULT 0,
    chunks TEXT NOT NULL DEFAULT '[]',
    expires_at BIGINT NOT NULL
);

CREATE TABLE package_registry_tokens (
    hash TEXT PRIMARY KEY,
    token_id BIGINT REFERENCES personal_access_tokens(id) ON DELETE CASCADE,
    resources TEXT NOT NULL,
    write BOOLEAN NOT NULL,
    expires_at BIGINT NOT NULL,
    CHECK (token_id IS NOT NULL OR NOT write)
);

-- projects
CREATE TABLE projects (
    id BIGSERIAL PRIMARY KEY,
    owner_user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    owner_kind TEXT GENERATED ALWAYS AS (CASE WHEN owner_org_id IS NULL THEN 'user' ELSE 'org' END) STORED,
    owner_id BIGINT GENERATED ALWAYS AS (COALESCE(owner_user_id, owner_org_id)) STORED,
    number BIGINT NOT NULL,
    creator_id BIGINT NOT NULL REFERENCES users(id),
    title TEXT NOT NULL,
    description TEXT,
    public BOOLEAN NOT NULL DEFAULT false,
    closed_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    readme TEXT NOT NULL DEFAULT '',
    template BOOLEAN NOT NULL DEFAULT false,
    CHECK ((owner_user_id IS NULL) <> (owner_org_id IS NULL)),
    UNIQUE(owner_kind, owner_id, number)
);

CREATE TABLE project_fields (
    id BIGSERIAL PRIMARY KEY,
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    data_type TEXT NOT NULL CHECK (data_type IN ('title','text','number','date','single_select','iteration')),
    configuration TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE(project_id, name)
);

-- migrations
CREATE TABLE repository_imports (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    generation TEXT NOT NULL,
    vcs_url TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('importing','complete','error')),
    updated_at BIGINT NOT NULL
);

CREATE TABLE pages_sites (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    build_type TEXT NOT NULL CHECK (build_type IN ('legacy','workflow')),
    branch TEXT, path TEXT CHECK (path IN ('/','/docs')), cname TEXT UNIQUE,
    https_enforced BOOLEAN NOT NULL DEFAULT FALSE, is_public BOOLEAN NOT NULL DEFAULT FALSE,
    CHECK ((branch IS NULL) = (path IS NULL))
);

CREATE TABLE pages_builds (
    id BIGSERIAL PRIMARY KEY, repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL, commit_sha TEXT NOT NULL,
    status TEXT NOT NULL, error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX pages_builds_repo ON pages_builds(repo_id,id);

CREATE TABLE pages_deployments (
    id BIGSERIAL PRIMARY KEY, repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL, environment TEXT NOT NULL, status TEXT NOT NULL,
    artifact_id BIGINT, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX pages_deployments_repo ON pages_deployments(repo_id,id);

CREATE TABLE repository_private_reporting (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    enabled_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE repository_security_reports (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    reporter_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    title TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 255),
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65536),
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','closed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_security_reports_recent ON repository_security_reports(repo_id,id DESC);
CREATE INDEX idx_security_reports_reporter ON repository_security_reports(repo_id,reporter_id,created_at);

CREATE TABLE repository_security_replies (
    id BIGSERIAL PRIMARY KEY,
    report_id BIGINT NOT NULL REFERENCES repository_security_reports(id) ON DELETE CASCADE,
    author_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    body TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65536),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_security_replies_report ON repository_security_replies(report_id,id);

CREATE TABLE repository_advisories (
    id BIGSERIAL PRIMARY KEY, ghsa_id TEXT NOT NULL UNIQUE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    author_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('triage','draft','published','closed')),
    document TEXT NOT NULL,
    report_id BIGINT UNIQUE REFERENCES repository_security_reports(id) ON DELETE SET NULL,
    private_fork_id BIGINT UNIQUE REFERENCES repositories(id) ON DELETE SET NULL,
    cve_requested_at TIMESTAMPTZ, published_at TIMESTAMPTZ, closed_at TIMESTAMPTZ,
    publisher_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    report_accepted BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX repository_advisories_repo ON repository_advisories(repo_id,id);

-- dependabot
CREATE TABLE repo_dependabot_keys (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BYTEA NOT NULL CHECK (octet_length(encrypted_key)=48),
    nonce BYTEA NOT NULL CHECK (octet_length(nonce)=12)
);

CREATE TABLE repo_dependabot_secrets (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    encrypted_value BYTEA NOT NULL,
    nonce BYTEA NOT NULL CHECK (octet_length(nonce)=12),
    created_by_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(repo_id, name)
);

CREATE TABLE org_dependabot_keys (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BYTEA NOT NULL CHECK (octet_length(encrypted_key)=48),
    nonce BYTEA NOT NULL CHECK (octet_length(nonce)=12)
);

CREATE TABLE org_dependabot_secrets (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    visibility TEXT NOT NULL CHECK (visibility IN ('all','private','selected')),
    encrypted_value BYTEA NOT NULL,
    nonce BYTEA NOT NULL CHECK (octet_length(nonce)=12),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(org_id, name)
);

CREATE TABLE org_dependabot_secret_repositories (
    org_id BIGINT NOT NULL,
    name TEXT NOT NULL,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    FOREIGN KEY(org_id, name) REFERENCES org_dependabot_secrets(org_id, name) ON DELETE CASCADE,
    PRIMARY KEY(org_id, name, repo_id)
);
CREATE INDEX org_dependabot_secret_repositories_repo ON org_dependabot_secret_repositories(repo_id);

-- Dependency graph
CREATE TABLE dependency_snapshots (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    reference TEXT NOT NULL,
    correlator TEXT NOT NULL,
    detector TEXT NOT NULL,
    scanned TIMESTAMPTZ NOT NULL,
    body TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX dependency_snapshots_reference ON dependency_snapshots(repo_id,reference,detector,correlator,scanned DESC,id DESC);
CREATE INDEX dependency_snapshots_sha ON dependency_snapshots(repo_id,sha,detector,correlator,scanned DESC,id DESC);

CREATE TABLE dependency_sbom_reports (
    id TEXT PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX dependency_sbom_reports_repository ON dependency_sbom_reports(repo_id,created_at);

-- actions-admin: organization and environment values.
CREATE TABLE org_actions_keys (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL UNIQUE,
    encrypted_key BYTEA NOT NULL CHECK (octet_length(encrypted_key)=48),
    nonce BYTEA NOT NULL CHECK (octet_length(nonce)=12)
);

CREATE TABLE org_actions_values (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('secret','variable')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255 AND name=upper(name)),
    value TEXT NOT NULL DEFAULT '' CHECK (octet_length(value)<=49152),
    encrypted_value BYTEA,
    nonce BYTEA,
    visibility TEXT NOT NULL CHECK (visibility IN ('all','private','selected')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(org_id,kind,name),
    CHECK ((kind='secret' AND value='' AND encrypted_value IS NOT NULL AND octet_length(encrypted_value) BETWEEN 16 AND 49168 AND nonce IS NOT NULL AND octet_length(nonce)=12) OR (kind='variable' AND encrypted_value IS NULL AND nonce IS NULL))
);

CREATE TABLE org_actions_value_repositories (
    org_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(org_id,kind,name,repo_id),
    FOREIGN KEY(org_id,kind,name) REFERENCES org_actions_values(org_id,kind,name) ON DELETE CASCADE ON UPDATE CASCADE
);
CREATE INDEX org_actions_value_repo ON org_actions_value_repositories(repo_id);

CREATE TABLE repo_environment_values (
    repo_id BIGINT NOT NULL,
    environment TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('secret','variable')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255 AND name=upper(name)),
    value TEXT NOT NULL DEFAULT '' CHECK (octet_length(value)<=49152),
    encrypted_value BYTEA,
    nonce BYTEA,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY(repo_id,environment,kind,name),
    CHECK ((kind='secret' AND value='' AND encrypted_value IS NOT NULL AND octet_length(encrypted_value) BETWEEN 16 AND 49168 AND nonce IS NOT NULL AND octet_length(nonce)=12) OR (kind='variable' AND encrypted_value IS NULL AND nonce IS NULL)),
    FOREIGN KEY(repo_id,environment) REFERENCES deployment_environments(repo_id,name_key) ON DELETE CASCADE ON UPDATE CASCADE
);

-- actions-admin: signed attestation evidence, not a trust-verification verdict.
CREATE TABLE attestation_bundles (
    hash TEXT PRIMARY KEY,
    bundle TEXT NOT NULL CHECK (jsonb_typeof(bundle::jsonb)='object' AND octet_length(bundle) BETWEEN 1 AND 1048576)
);

CREATE TABLE artifact_attestations (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    bundle_hash TEXT NOT NULL REFERENCES attestation_bundles(hash),
    predicate_type TEXT NOT NULL CHECK (octet_length(predicate_type) BETWEEN 1 AND 2048),
    initiator TEXT NOT NULL CHECK (initiator IN ('user','workflow')),
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(repo_id,bundle_hash)
);

CREATE TABLE attestation_subjects (
    attestation_id BIGINT NOT NULL REFERENCES artifact_attestations(id) ON DELETE CASCADE,
    digest TEXT NOT NULL CHECK (digest ~ '^(sha256:[0-9a-f]{64}|sha512:[0-9a-f]{128})$'),
    PRIMARY KEY(attestation_id,digest)
);
CREATE INDEX attestation_subject_digest ON attestation_subjects(digest,attestation_id);
CREATE INDEX attestation_bundle_references ON artifact_attestations(bundle_hash);
CREATE VIEW repo_attestations AS SELECT a.id,a.repo_id,b.bundle,a.predicate_type,a.initiator,a.created_at FROM artifact_attestations a JOIN attestation_bundles b ON b.hash=a.bundle_hash;

CREATE TABLE artifact_metadata (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    digest TEXT NOT NULL CHECK (length(digest)=71 AND left(digest,7)='sha256:'),
    name TEXT NOT NULL,
    version TEXT,
    UNIQUE(repo_id,digest)
);
CREATE INDEX artifact_metadata_digest ON artifact_metadata(digest,repo_id);

CREATE TABLE artifact_metadata_records (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    artifact_id BIGINT NOT NULL REFERENCES artifact_metadata(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('deployment','storage')),
    identity TEXT NOT NULL,
    cluster TEXT NOT NULL DEFAULT '',
    logical_environment TEXT NOT NULL DEFAULT '',
    physical_environment TEXT NOT NULL DEFAULT '',
    document TEXT NOT NULL,
    attestation_id BIGINT REFERENCES artifact_attestations(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(org_id,kind,identity)
);
CREATE INDEX artifact_metadata_record_artifact ON artifact_metadata_records(artifact_id,kind,id);
CREATE INDEX artifact_metadata_record_cluster ON artifact_metadata_records(org_id,cluster,logical_environment,physical_environment) WHERE kind='deployment';
CREATE INDEX artifact_metadata_record_attestation ON artifact_metadata_records(attestation_id) WHERE attestation_id IS NOT NULL;

CREATE TABLE artifact_metadata_jobs (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    cluster TEXT NOT NULL,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_id BIGINT NOT NULL,
    scoped_repo BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','processing','completed','failed')),
    started_at TIMESTAMPTZ,
    total_count BIGINT,
    errors TEXT NOT NULL DEFAULT '[]'
);
CREATE UNIQUE INDEX artifact_metadata_active_job ON artifact_metadata_jobs(org_id,cluster) WHERE status IN ('pending','processing');
CREATE INDEX artifact_metadata_pending_jobs ON artifact_metadata_jobs(id) WHERE status='pending';

-- code-scanning
CREATE TABLE code_scanning_uploads (
    sequence BIGSERIAL PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    ref_name TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    sarif TEXT NOT NULL,
    checkout_uri TEXT,
    processing_status TEXT NOT NULL DEFAULT 'pending' CHECK(processing_status IN ('pending','complete','failed')),
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX code_scanning_uploads_pending ON code_scanning_uploads(processing_status,sequence);

CREATE TABLE code_scanning_analyses (
    id BIGSERIAL PRIMARY KEY,
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
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE(upload_id,tool_key,category)
);
CREATE INDEX code_scanning_analyses_repository ON code_scanning_analyses(repo_id,id);

CREATE TABLE code_scanning_alerts (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number BIGINT NOT NULL,
    fingerprint TEXT NOT NULL,
    tool_key TEXT NOT NULL,
    tool TEXT NOT NULL,
    rule TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open','fixed','dismissed')),
    dismissed_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    dismissed_at TIMESTAMPTZ,
    dismissed_reason TEXT,
    dismissed_comment TEXT,
    fixed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
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
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    settings TEXT NOT NULL,
    default_for_new_repos TEXT NOT NULL DEFAULT 'none' CHECK(default_for_new_repos IN ('all','none','public','private_and_internal')),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE(org_id,name)
);

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
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    removed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY(org_id,user_id)
);
CREATE UNIQUE INDEX email_outbox_organization_invitation ON email_outbox(organization_invitation_id) WHERE organization_invitation_id IS NOT NULL;

CREATE TABLE organization_blocks (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY(org_id,user_id)
);
CREATE INDEX organization_blocks_user ON organization_blocks(user_id);

CREATE TABLE organization_roles (
    id BIGINT PRIMARY KEY CHECK (id>0),
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    base_level SMALLINT NOT NULL CHECK (base_level BETWEEN 1 AND 5),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
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
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    role_id BIGINT NOT NULL REFERENCES organization_roles(id),
    user_id BIGINT,
    team_id BIGINT,
    CHECK ((user_id IS NULL) <> (team_id IS NULL)),
    FOREIGN KEY(org_id,user_id) REFERENCES org_memberships(org_id,user_id) ON DELETE CASCADE,
    FOREIGN KEY(org_id,team_id) REFERENCES teams(org_id,id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX organization_role_users ON organization_role_assignments(org_id,user_id,role_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX organization_role_teams ON organization_role_assignments(org_id,team_id,role_id) WHERE team_id IS NOT NULL;
CREATE INDEX organization_role_members ON organization_role_assignments(org_id,role_id);

-- Organization repository property definitions and tenant-scoped explicit values.
CREATE TABLE organization_repository_properties (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    property_name TEXT NOT NULL CHECK(property_name ~ '^[A-Za-z0-9_-]{1,75}$'),
    value_type TEXT NOT NULL CHECK(value_type IN ('string','single_select','multi_select','true_false','url')),
    required BOOLEAN NOT NULL DEFAULT FALSE,
    default_value TEXT CHECK(default_value IS NULL OR json_typeof(default_value::JSON) IN ('string','array')),
    description TEXT,
    allowed_values TEXT CHECK(allowed_values IS NULL OR json_typeof(allowed_values::JSON)='array'),
    values_editable_by TEXT NOT NULL DEFAULT 'org_actors' CHECK(values_editable_by IN ('org_actors','org_and_repo_actors')),
    require_explicit_values BOOLEAN NOT NULL DEFAULT FALSE,
    CHECK(value_type IN ('single_select','multi_select') OR allowed_values IS NULL),
    UNIQUE(org_id,id)
);
CREATE UNIQUE INDEX organization_repository_property_names ON organization_repository_properties(org_id,LOWER(property_name));
CREATE UNIQUE INDEX repository_properties_owner_id ON repositories(owner_org_id,id);

CREATE TABLE repository_property_values (
    org_id BIGINT NOT NULL,
    repo_id BIGINT NOT NULL,
    definition_id BIGINT NOT NULL,
    value TEXT NOT NULL CHECK(json_typeof(value::JSON) IN ('string','array')),
    PRIMARY KEY(repo_id,definition_id),
    FOREIGN KEY(org_id,repo_id) REFERENCES repositories(owner_org_id,id) ON DELETE CASCADE,
    FOREIGN KEY(org_id,definition_id) REFERENCES organization_repository_properties(org_id,id) ON DELETE CASCADE
);
CREATE INDEX repository_property_definition_values ON repository_property_values(definition_id,repo_id);

CREATE TABLE organization_settings (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    company TEXT,
    email TEXT,
    twitter_username TEXT,
    location TEXT,
    blog TEXT,
    has_organization_projects BOOLEAN NOT NULL DEFAULT TRUE,
    default_repository_permission SMALLINT NOT NULL DEFAULT 0 CHECK(default_repository_permission IN (0,1,3,5)),
    members_can_create_public_repositories BOOLEAN NOT NULL DEFAULT FALSE,
    members_can_create_private_repositories BOOLEAN NOT NULL DEFAULT FALSE,
    members_can_fork_private_repositories BOOLEAN NOT NULL DEFAULT TRUE,
    web_commit_signoff_required BOOLEAN NOT NULL DEFAULT FALSE,
    deploy_keys_enabled_for_repositories BOOLEAN NOT NULL DEFAULT TRUE
);

-- pulls: team identities remain requests as membership changes.
CREATE TABLE pr_team_reviewers (
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    as_code_owner BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY(pull_request_id, team_id)
);
CREATE INDEX pr_team_reviewers_team ON pr_team_reviewers(team_id);

CREATE TABLE repository_exports (
    id BIGSERIAL PRIMARY KEY,
    guid TEXT NOT NULL UNIQUE,
    owner_user_id BIGINT REFERENCES users(id) ON DELETE CASCADE,
    owner_org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    repositories TEXT NOT NULL CHECK (jsonb_typeof(repositories::jsonb)='array'),
    options TEXT NOT NULL CHECK (jsonb_typeof(options::jsonb)='object'),
    state TEXT NOT NULL CHECK (state IN ('pending','exporting','exported','failed')),
    archive_deleted BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    CHECK ((owner_user_id IS NOT NULL) <> (owner_org_id IS NOT NULL))
);

CREATE TABLE repository_migration_locks (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    migration_id BIGINT NOT NULL REFERENCES repository_exports(id) ON DELETE CASCADE
);
CREATE INDEX repository_exports_user ON repository_exports(owner_user_id,id);
CREATE INDEX repository_exports_org ON repository_exports(owner_org_id,id);
CREATE FUNCTION repository_exports_cleanup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO pending_object_deletions(object_key,delete_after)
    VALUES('migrations/' || OLD.guid,CURRENT_TIMESTAMP + INTERVAL '1 hour')
    ON CONFLICT(object_key) DO UPDATE SET delete_after=GREATEST(pending_object_deletions.delete_after,excluded.delete_after);
    RETURN OLD;
END;
$$;
CREATE TRIGGER repository_exports_cleanup BEFORE DELETE ON repository_exports
FOR EACH ROW EXECUTE FUNCTION repository_exports_cleanup();

CREATE TABLE project_items (
    id BIGSERIAL PRIMARY KEY,
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    creator_id BIGINT NOT NULL REFERENCES users(id),
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    issue_id BIGINT REFERENCES issues(id) ON DELETE CASCADE,
    pull_request_id BIGINT REFERENCES pull_requests(id) ON DELETE CASCADE,
    draft_title TEXT,
    draft_body TEXT,
    field_values TEXT NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    archived BOOLEAN NOT NULL DEFAULT false,
    position BIGINT NOT NULL DEFAULT 0,
    CHECK ((issue_id IS NOT NULL AND pull_request_id IS NULL AND draft_title IS NULL AND repo_id IS NOT NULL)
        OR (pull_request_id IS NOT NULL AND issue_id IS NULL AND draft_title IS NULL AND repo_id IS NOT NULL)
        OR (draft_title IS NOT NULL AND issue_id IS NULL AND pull_request_id IS NULL AND repo_id IS NULL)),
    UNIQUE(project_id, issue_id),
    UNIQUE(project_id, pull_request_id)
);

CREATE TABLE project_views (
    id BIGSERIAL PRIMARY KEY,
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number BIGINT NOT NULL,
    creator_id BIGINT NOT NULL REFERENCES users(id),
    configuration TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE(project_id, number)
);

CREATE TABLE project_repositories (
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(project_id, repo_id)
);

CREATE TABLE github_api_limits (
    requester TEXT NOT NULL,
    resource TEXT NOT NULL CHECK (resource IN ('core','search')),
    used BIGINT NOT NULL CHECK (used>=0),
    reset_at BIGINT NOT NULL,
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
    encrypted_secret BYTEA NOT NULL,
    nonce BYTEA NOT NULL,
    assigned_to BIGINT REFERENCES users(id) ON DELETE SET NULL,
    validity TEXT CHECK (validity IN ('active','inactive')),
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','resolved')),
    resolution TEXT CHECK (resolution IN ('false_positive','wont_fix','revoked','used_in_tests')),
    resolution_comment TEXT,
    resolved_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    resolved_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
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
    -- SHA-256 of the path. The key uses it because a long path can exceed a B-tree entry.
    path_digest TEXT NOT NULL,
    start_line BIGINT NOT NULL,
    end_line BIGINT NOT NULL,
    start_column BIGINT NOT NULL,
    end_column BIGINT NOT NULL,
    PRIMARY KEY (repo_id, alert_number, commit_sha, path_digest, start_line, start_column),
    FOREIGN KEY (repo_id, alert_number) REFERENCES secret_scanning_alerts(repo_id, number) ON DELETE CASCADE
);

CREATE TABLE secret_scanning_jobs (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    head_sha TEXT,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    scan_type TEXT NOT NULL CHECK (scan_type IN ('backfill','incremental','pattern_update','custom_pattern_backfill')),
    pattern_name TEXT,
    pattern_scope TEXT CHECK (pattern_scope IN ('repository','organization')),
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','completed','failed')),
    -- Counts claims, so a worker whose claim expired cannot report over its replacement.
    attempt BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    heartbeat_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);
CREATE INDEX secret_scanning_jobs_pending ON secret_scanning_jobs(status, id);

CREATE TABLE secret_scanning_commits (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL,
    PRIMARY KEY (repo_id, commit_sha)
);

CREATE TABLE secret_scanning_bypasses (
    id TEXT PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL,
    token_type TEXT NOT NULL,
    reason TEXT CHECK (reason IN ('false_positive','used_in_tests','will_fix_later')),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ
);
CREATE INDEX secret_scanning_bypasses_value ON secret_scanning_bypasses(repo_id, actor_id, secret_hash);

CREATE TABLE secret_scanning_patterns (
    id BIGSERIAL PRIMARY KEY,
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

-- Repository wiki setting.
CREATE TABLE repository_features (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    wiki_enabled BOOLEAN NOT NULL DEFAULT FALSE
);

-- discussions
CREATE TABLE repository_discussion_settings (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT false
);

CREATE TABLE discussion_categories (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    emoji TEXT NOT NULL,
    description TEXT NOT NULL,
    answerable BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(repo_id,slug),
    UNIQUE(id,repo_id)
);

CREATE TABLE discussions (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    author_id BIGINT NOT NULL REFERENCES users(id),
    category_id BIGINT NOT NULL,
    locked BOOLEAN NOT NULL DEFAULT false,
    answer_id BIGINT,
    answer_chosen_at TIMESTAMPTZ,
    answer_chosen_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY(category_id,repo_id) REFERENCES discussion_categories(id,repo_id),
    UNIQUE(repo_id,number)
);

CREATE TABLE discussion_comments (
    id BIGSERIAL PRIMARY KEY,
    discussion_id BIGINT NOT NULL REFERENCES discussions(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL REFERENCES users(id),
    body TEXT NOT NULL,
    reply_to_id BIGINT REFERENCES discussion_comments(id) ON DELETE CASCADE,
    is_answer BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);
ALTER TABLE discussions ADD FOREIGN KEY(answer_id) REFERENCES discussion_comments(id) ON DELETE SET NULL;
CREATE INDEX discussion_comments_thread ON discussion_comments(discussion_id,reply_to_id,id);
CREATE UNIQUE INDEX discussion_one_answer ON discussion_comments(discussion_id) WHERE is_answer;

CREATE TABLE discussion_labels (
    discussion_id BIGINT NOT NULL REFERENCES discussions(id) ON DELETE CASCADE,
    label_id BIGINT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY(discussion_id,label_id)
);

-- vulnerability scanning
CREATE TABLE vulnerability_settings (
    id BIGINT PRIMARY KEY CHECK(id=1),
    base_url TEXT NOT NULL DEFAULT 'https://osv-vulnerabilities.storage.googleapis.com',
    ecosystems TEXT NOT NULL DEFAULT '[]',
    next_sync TIMESTAMPTZ NOT NULL DEFAULT '1970-01-01T00:00:00Z'
);
INSERT INTO vulnerability_settings(id) VALUES(1);

CREATE TABLE osv_advisories (
    id TEXT PRIMARY KEY,
    modified TIMESTAMPTZ NOT NULL,
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
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    generation BIGINT NOT NULL,
    next_scan TIMESTAMPTZ NOT NULL,
    lease_until TIMESTAMPTZ
);

-- Actions job annotations
CREATE TABLE actions_job_annotations (
    id BIGSERIAL PRIMARY KEY,
    job_id BIGINT NOT NULL REFERENCES actions_jobs(id) ON DELETE CASCADE,
    annotation JSONB NOT NULL
);
CREATE INDEX actions_job_annotations_job ON actions_job_annotations(job_id,id);

-- CodeQL databases
CREATE TABLE codeql_databases (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    language TEXT NOT NULL,
    name TEXT NOT NULL,
    uploader_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    object_key TEXT NOT NULL UNIQUE,
    size BIGINT NOT NULL CHECK (size > 0),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE(repo_id, language)
);
CREATE FUNCTION codeql_database_cleanup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' OR OLD.object_key <> NEW.object_key THEN
        INSERT INTO pending_object_deletions(object_key,delete_after)
        VALUES(OLD.object_key,NOW()+INTERVAL '7 days') ON CONFLICT(object_key) DO NOTHING;
    END IF;
    RETURN OLD;
END;
$$;
CREATE TRIGGER codeql_database_cleanup AFTER DELETE OR UPDATE OF object_key ON codeql_databases
FOR EACH ROW EXECUTE FUNCTION codeql_database_cleanup();

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
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    installation_id BIGINT,
    UNIQUE (org_id, display_name)
);

CREATE TABLE organization_external_property_values (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    property_name TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (org_id, repo_id, property_name)
);

CREATE TABLE repository_import_authors (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    author_name TEXT NOT NULL,
    author_email TEXT NOT NULL,
    mapped_name TEXT,
    mapped_email TEXT,
    UNIQUE (repo_id, author_name, author_email)
);

CREATE TABLE project_teams (
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    PRIMARY KEY(project_id, team_id)
);

-- checks: publisher-owned runs, annotations and repository preferences.
CREATE TABLE check_suites (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    publisher TEXT NOT NULL,
    app_id BIGINT,
    -- A workflow run's suite; its status and conclusion are read from the run.
    ci_run_id BIGINT UNIQUE REFERENCES ci_runs(id) ON DELETE CASCADE,
    head_sha TEXT NOT NULL,
    head_branch TEXT,
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','in_progress','completed')),
    conclusion TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE(repo_id,publisher,head_sha)
);
CREATE INDEX check_suites_ref ON check_suites(repo_id,head_sha,id);

CREATE TABLE check_runs (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    suite_id BIGINT NOT NULL REFERENCES check_suites(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','in_progress','completed')),
    conclusion TEXT,
    data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX check_runs_latest ON check_runs(suite_id,name,id);
CREATE INDEX check_runs_repo ON check_runs(repo_id,id);

CREATE TABLE check_annotations (
    id BIGSERIAL PRIMARY KEY,
    run_id BIGINT NOT NULL REFERENCES check_runs(id) ON DELETE CASCADE,
    data JSONB NOT NULL
);
CREATE INDEX check_annotations_run ON check_annotations(run_id,id);

CREATE TABLE check_suite_preferences (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    app_id BIGINT NOT NULL REFERENCES oauth_applications(id) ON DELETE CASCADE,
    setting BOOLEAN NOT NULL,
    PRIMARY KEY(repo_id,app_id)
);

CREATE TABLE actions_policies (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    data TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ((repo_id IS NULL) <> (org_id IS NULL))
);
CREATE INDEX actions_policies_repo ON actions_policies(repo_id);
CREATE INDEX actions_policies_org ON actions_policies(org_id);

CREATE TABLE actions_deployment_reviews (
    id BIGSERIAL PRIMARY KEY,
    run_id BIGINT NOT NULL REFERENCES ci_runs(id) ON DELETE CASCADE,
    environment_id BIGINT NOT NULL REFERENCES deployment_environments(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('approved','rejected')),
    comment TEXT NOT NULL,
    deployment_id BIGINT REFERENCES repository_deployments(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(run_id,environment_id)
);

-- repos: collaborator invitations.
CREATE TABLE repository_invitations (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    invitee_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    inviter_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    permission SMALLINT NOT NULL CHECK (permission BETWEEN 1 AND 5),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','accepted','declined','cancelled','expired')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX repository_invitations_pending ON repository_invitations(repo_id,invitee_id) WHERE state='pending';
CREATE INDEX repository_invitations_recipient ON repository_invitations(invitee_id,state,created_at);
CREATE INDEX repository_invitations_daily ON repository_invitations(repo_id,created_at);

-- Lookup only: releases can hold several forks of one source in an organization, so creation
-- and transfer refuse a second one under the source row lock.
CREATE INDEX idx_repositories_org_fork_source ON repositories(owner_org_id,forked_from_id) WHERE owner_kind='org' AND forked_from_id IS NOT NULL;

CREATE TABLE repository_autolinks (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    key_prefix TEXT NOT NULL CHECK (length(key_prefix) BETWEEN 1 AND 80),
    url_template TEXT NOT NULL CHECK (length(url_template) BETWEEN 1 AND 4096),
    is_alphanumeric BOOLEAN NOT NULL DEFAULT TRUE,
    UNIQUE(repo_id,key_prefix)
);

CREATE TABLE repository_ref_activities (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    operation_id TEXT NOT NULL CHECK (length(operation_id)=36),
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 1024),
    before_sha TEXT NOT NULL CHECK (length(before_sha)=40),
    after_sha TEXT NOT NULL CHECK (length(after_sha)=40),
    activity_type TEXT NOT NULL CHECK (activity_type IN ('push','force_push','branch_creation','branch_deletion','pr_merge','merge_queue_merge')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id,operation_id,reference)
);
CREATE INDEX idx_ref_activities_recent ON repository_ref_activities(repo_id,id DESC);
CREATE INDEX idx_ref_activities_reference ON repository_ref_activities(repo_id,reference,id);
CREATE INDEX idx_ref_activities_actor ON repository_ref_activities(repo_id,actor_id,id);

CREATE TABLE repository_traffic_keys (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day TEXT NOT NULL CHECK(length(day)=10),
    salt TEXT NOT NULL CHECK(length(salt)=64),
    PRIMARY KEY(repo_id,day)
);

CREATE TABLE repository_traffic_visitors (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day TEXT NOT NULL CHECK(length(day)=10),
    fingerprint TEXT NOT NULL CHECK(length(fingerprint)=64),
    visitor TEXT NOT NULL,
    PRIMARY KEY(repo_id,day,fingerprint)
);

CREATE TABLE repository_traffic_counts (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    day TEXT NOT NULL CHECK(length(day)=10),
    kind TEXT NOT NULL CHECK(kind IN ('view','clone')),
    visitor TEXT NOT NULL,
    path_hash TEXT NOT NULL CHECK(length(path_hash)=64),
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    referrer TEXT NOT NULL,
    count BIGINT NOT NULL CHECK(count>0),
    last_seen TIMESTAMPTZ NOT NULL,
    PRIMARY KEY(repo_id,day,kind,visitor,path_hash,referrer)
);
CREATE INDEX idx_repository_traffic_counts_window ON repository_traffic_counts(repo_id,kind,day);
CREATE INDEX idx_repository_traffic_counts_paths ON repository_traffic_counts(repo_id,kind,path_hash,last_seen DESC);
CREATE INDEX idx_repository_traffic_counts_retention ON repository_traffic_counts(day);
CREATE INDEX idx_repository_traffic_visitors_retention ON repository_traffic_visitors(day);
CREATE INDEX idx_repository_traffic_keys_retention ON repository_traffic_keys(day);

CREATE TABLE repository_immutable_releases (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE
);

-- Publication and namespace history outlive deletion, so an old immutable tag cannot be reused.
CREATE TABLE immutable_release_publications (
    repo_id BIGINT NOT NULL,
    tag_name TEXT NOT NULL CHECK(length(tag_name) BETWEEN 1 AND 255),
    tag_oid TEXT NOT NULL CHECK(length(tag_oid)=40),
    commit_sha TEXT NOT NULL CHECK(length(commit_sha)=40),
    release_id BIGINT UNIQUE REFERENCES releases(id) ON DELETE SET NULL,
    signer_key TEXT NOT NULL CHECK(length(signer_key)=64),
    attestation TEXT NOT NULL CHECK(length(attestation)<=2097152),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY(repo_id,tag_name)
);

CREATE TABLE immutable_release_repository_names (
    repo_id BIGINT NOT NULL,
    owner_kind TEXT NOT NULL CHECK(owner_kind IN ('user','org')),
    owner_id BIGINT NOT NULL,
    name_key TEXT NOT NULL,
    PRIMARY KEY(repo_id,owner_kind,owner_id,name_key)
);
CREATE INDEX idx_immutable_release_repository_names_owner ON immutable_release_repository_names(owner_kind,owner_id,name_key);

CREATE TABLE immutable_release_owner_names (
    owner_kind TEXT NOT NULL CHECK(owner_kind IN ('user','org')),
    owner_id BIGINT NOT NULL,
    name_key TEXT NOT NULL,
    PRIMARY KEY(owner_kind,owner_id,name_key)
);
CREATE INDEX idx_immutable_release_owner_names_name ON immutable_release_owner_names(name_key);
CREATE FUNCTION immutable_release_capture_names() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME='repositories' THEN
        IF EXISTS(SELECT 1 FROM immutable_release_publications WHERE repo_id=NEW.id) THEN
            INSERT INTO immutable_release_repository_names(repo_id,owner_kind,owner_id,name_key)
            VALUES(NEW.id,NEW.owner_kind,COALESCE(NEW.owner_user_id,NEW.owner_org_id),LOWER(NEW.name)) ON CONFLICT DO NOTHING;
            INSERT INTO immutable_release_owner_names(owner_kind,owner_id,name_key)
            SELECT NEW.owner_kind,COALESCE(NEW.owner_user_id,NEW.owner_org_id),LOWER(COALESCE(u.username,g.name))
            FROM repositories r LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations g ON g.id=r.owner_org_id WHERE r.id=NEW.id ON CONFLICT DO NOTHING;
        END IF;
    ELSIF TG_TABLE_NAME='users' THEN
        IF EXISTS(SELECT 1 FROM immutable_release_owner_names WHERE owner_kind='user' AND owner_id=NEW.id) THEN
            INSERT INTO immutable_release_owner_names(owner_kind,owner_id,name_key) VALUES('user',NEW.id,LOWER(NEW.username)) ON CONFLICT DO NOTHING;
        END IF;
    ELSIF EXISTS(SELECT 1 FROM immutable_release_owner_names WHERE owner_kind='org' AND owner_id=NEW.id) THEN
        INSERT INTO immutable_release_owner_names(owner_kind,owner_id,name_key) VALUES('org',NEW.id,LOWER(NEW.name)) ON CONFLICT DO NOTHING;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER immutable_release_capture_repository AFTER UPDATE OF name,owner_kind,owner_user_id,owner_org_id ON repositories FOR EACH ROW EXECUTE FUNCTION immutable_release_capture_names();
CREATE TRIGGER immutable_release_capture_user AFTER UPDATE OF username ON users FOR EACH ROW EXECUTE FUNCTION immutable_release_capture_names();
CREATE TRIGGER immutable_release_capture_organization AFTER UPDATE OF name ON organizations FOR EACH ROW EXECUTE FUNCTION immutable_release_capture_names();
CREATE FUNCTION immutable_release_guard_owner_name() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE wanted_kind TEXT; wanted_name TEXT;
BEGIN
    IF TG_TABLE_NAME='users' THEN wanted_kind:='user'; wanted_name:=NEW.username;
    ELSE wanted_kind:='org'; wanted_name:=NEW.name; END IF;
    IF EXISTS(SELECT 1 FROM repositories c JOIN immutable_release_repository_names n ON n.name_key=LOWER(c.name) AND n.repo_id<>c.id JOIN immutable_release_owner_names o ON o.owner_kind=n.owner_kind AND o.owner_id=n.owner_id WHERE ((wanted_kind='user' AND c.owner_user_id=NEW.id) OR (wanted_kind='org' AND c.owner_org_id=NEW.id)) AND o.name_key=LOWER(wanted_name) AND NOT(o.owner_kind=wanted_kind AND o.owner_id=NEW.id)) THEN
        RAISE EXCEPTION 'Destination owner name has reserved immutable repository history; move conflicting repositories first' USING ERRCODE='23514',CONSTRAINT='immutable_release_namespace';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER immutable_release_guard_user BEFORE UPDATE OF username ON users FOR EACH ROW WHEN(LOWER(OLD.username)<>LOWER(NEW.username)) EXECUTE FUNCTION immutable_release_guard_owner_name();
CREATE TRIGGER immutable_release_guard_organization BEFORE UPDATE OF name ON organizations FOR EACH ROW WHEN(LOWER(OLD.name)<>LOWER(NEW.name)) EXECUTE FUNCTION immutable_release_guard_owner_name();

-- repos dependency security
CREATE TABLE repository_dependency_security (
    repo_id BIGINT PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    alerts_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    updates_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    alerts_actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    updates_actor_id BIGINT REFERENCES users(id) ON DELETE SET NULL,
    owner_kind TEXT NOT NULL CHECK(owner_kind IN ('user','org')),
    owner_id BIGINT NOT NULL CHECK(owner_id>0),
    private_consent BOOLEAN NOT NULL DEFAULT FALSE,
    generation BIGINT NOT NULL DEFAULT 1 CHECK(generation>0),
    next_scan_at TIMESTAMPTZ,
    lease_nonce TEXT CHECK(lease_nonce IS NULL OR length(lease_nonce)=36),
    lease_until TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 20),
    last_status TEXT NOT NULL DEFAULT 'idle' CHECK(last_status IN ('idle','queued','scanning','complete','partial','unsupported','error','paused')),
    last_error TEXT CHECK(last_error IS NULL OR length(last_error)<=1024),
    last_scan_sha TEXT CHECK(last_scan_sha IS NULL OR length(last_scan_sha)=40),
    last_scan_at TIMESTAMPTZ,
    last_attempt_at TIMESTAMPTZ,
    snapshot TEXT NOT NULL DEFAULT '{}' CHECK(jsonb_typeof(snapshot::jsonb)='object' AND OCTET_LENGTH(snapshot)<=4194304),
    CHECK(NOT updates_enabled OR alerts_enabled),
    CHECK((lease_nonce IS NULL)=(lease_until IS NULL))
);
CREATE INDEX idx_dependency_security_due ON repository_dependency_security(next_scan_at,repo_id) WHERE alerts_enabled;
CREATE FUNCTION dependency_security_reauthorize() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE repository_dependency_security SET alerts_actor_id=NULL,updates_enabled=FALSE,updates_actor_id=NULL,private_consent=FALSE,generation=generation+1,lease_nonce=NULL,lease_until=NULL,next_scan_at=NULL,last_status='paused',last_error='Repository ownership or privacy changed; a current administrator must reauthorize dependency security.' WHERE repo_id=NEW.id AND alerts_enabled;
    RETURN NEW;
END $$;
CREATE TRIGGER dependency_security_reauthorize AFTER UPDATE OF owner_kind,owner_user_id,owner_org_id,is_private ON repositories FOR EACH ROW WHEN(OLD.owner_kind IS DISTINCT FROM NEW.owner_kind OR OLD.owner_user_id IS DISTINCT FROM NEW.owner_user_id OR OLD.owner_org_id IS DISTINCT FROM NEW.owner_org_id OR (NOT OLD.is_private AND NEW.is_private)) EXECUTE FUNCTION dependency_security_reauthorize();
CREATE FUNCTION dependency_security_default() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE repository_dependency_security SET generation=generation+1,next_scan_at=NOW(),last_status=CASE WHEN lease_nonce IS NULL THEN 'queued' ELSE last_status END WHERE repo_id=NEW.id AND alerts_enabled AND alerts_actor_id IS NOT NULL;
    RETURN NEW;
END $$;
CREATE TRIGGER dependency_security_default AFTER UPDATE OF default_branch ON repositories FOR EACH ROW WHEN(OLD.default_branch IS DISTINCT FROM NEW.default_branch) EXECUTE FUNCTION dependency_security_default();

-- Dependabot repository access
CREATE TABLE org_dependabot_access (
    org_id BIGINT PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    default_level TEXT NOT NULL CHECK(default_level IN ('public','internal'))
);

CREATE TABLE org_dependabot_access_repositories (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    PRIMARY KEY(org_id,repo_id)
);
CREATE INDEX org_dependabot_access_repositories_repo ON org_dependabot_access_repositories(repo_id);

-- Private registries
CREATE TABLE org_private_registries (
    org_id BIGINT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    configuration TEXT NOT NULL,
    encrypted_value BYTEA,
    nonce BYTEA CHECK(nonce IS NULL OR octet_length(nonce)=12),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
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
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number BIGINT NOT NULL,
    ghsa_id TEXT NOT NULL,
    manifest TEXT NOT NULL,
    ecosystem TEXT NOT NULL,
    package TEXT NOT NULL,
    version TEXT NOT NULL,
    scope TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('open','dismissed','fixed')),
    advisory TEXT NOT NULL,
    vulnerability TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    fixed_at TIMESTAMPTZ,
    dismissed_at TIMESTAMPTZ,
    dismissed_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    dismissed_reason TEXT,
    dismissed_comment TEXT,
    PRIMARY KEY(repo_id,number),
    UNIQUE(repo_id,ghsa_id,manifest,ecosystem,package)
);

-- Actions concurrency groups
CREATE TABLE actions_concurrency_groups (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name_key TEXT NOT NULL,
    name TEXT NOT NULL,
    last_acquired_at TIMESTAMPTZ,
    PRIMARY KEY(repo_id, name_key)
);

CREATE TABLE actions_run_concurrency (
    run_id BIGINT PRIMARY KEY REFERENCES ci_runs(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL,
    name_key TEXT NOT NULL,
    holding BOOLEAN NOT NULL DEFAULT FALSE,
    cancel_in_progress BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY(repo_id, name_key) REFERENCES actions_concurrency_groups(repo_id, name_key) ON DELETE CASCADE
);
CREATE INDEX actions_run_concurrency_group ON actions_run_concurrency(repo_id, name_key);

-- Organization rulesets
CREATE TABLE repository_rulesets (
    id BIGSERIAL PRIMARY KEY,
    owner_org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    owner_repo_id BIGINT REFERENCES repositories(id) ON DELETE CASCADE,
    definition TEXT NOT NULL CHECK(octet_length(definition)<=262144),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK((owner_org_id IS NOT NULL AND owner_repo_id IS NULL) OR (owner_org_id IS NULL AND owner_repo_id IS NOT NULL))
);
CREATE INDEX repository_rulesets_org ON repository_rulesets(owner_org_id,id);
CREATE INDEX repository_rulesets_repo ON repository_rulesets(owner_repo_id,id);

CREATE TABLE repository_ruleset_versions (
    id BIGSERIAL PRIMARY KEY,
    ruleset_id BIGINT NOT NULL REFERENCES repository_rulesets(id) ON DELETE CASCADE,
    definition TEXT NOT NULL CHECK(octet_length(definition)<=262144),
    -- Historical principal metadata, not a live authority grant; deletion does not rewrite it.
    actor_id BIGINT CHECK(actor_id IS NULL OR actor_id>0),
    actor_type TEXT NOT NULL CHECK(actor_type IN ('User','Integration','DeployKey','System')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX repository_ruleset_versions_rule ON repository_ruleset_versions(ruleset_id,id DESC);

CREATE TABLE repository_rule_suites (
    id BIGSERIAL PRIMARY KEY,
    org_id BIGINT REFERENCES organizations(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    repository_name TEXT NOT NULL CHECK(length(repository_name)<=255),
    actor_id BIGINT CHECK(actor_id IS NULL OR actor_id>0),
    actor_name TEXT CHECK(length(actor_name)<=255),
    ref_name TEXT NOT NULL CHECK(length(ref_name)<=1024),
    before_sha TEXT NOT NULL CHECK(length(before_sha) IN (40,64)),
    after_sha TEXT NOT NULL CHECK(length(after_sha) IN (40,64)),
    result TEXT NOT NULL CHECK(result IN ('pass','fail','bypass')),
    evaluation_result TEXT CHECK(evaluation_result IS NULL OR evaluation_result IN ('pass','fail','bypass')),
    evaluations TEXT NOT NULL CHECK(octet_length(evaluations)<=16777216),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX repository_rule_suites_org ON repository_rule_suites(org_id,id DESC);
CREATE INDEX repository_rule_suites_repo ON repository_rule_suites(repo_id,id DESC);

-- deployments: custom App protection rules and attempt-scoped decisions
CREATE TABLE deployment_protection_rules (
    id BIGSERIAL PRIMARY KEY,
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
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    issue_id BIGINT NOT NULL UNIQUE REFERENCES issues(id) ON DELETE CASCADE,
    slot INTEGER NOT NULL CHECK(slot BETWEEN 1 AND 3),
    PRIMARY KEY(repo_id, slot)
);

CREATE TABLE issue_redirects (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    issue_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    PRIMARY KEY(repo_id, number)
);

CREATE TABLE issue_duplicates (
    issue_id BIGINT PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE,
    duplicate_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    CHECK(issue_id <> duplicate_id)
);

CREATE TABLE issue_linked_branches (
    id BIGSERIAL PRIMARY KEY,
    issue_id BIGINT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
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
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    old_name TEXT NOT NULL,
    new_name TEXT NOT NULL,
    PRIMARY KEY(repo_id, old_name)
);

-- Pull request auto-merge: the background check merges once the requirements pass.
CREATE TABLE pull_request_auto_merges (
    pull_request_id BIGINT PRIMARY KEY REFERENCES pull_requests(id) ON DELETE CASCADE,
    enabled_by BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    merge_method TEXT NOT NULL CHECK(merge_method IN ('merge','squash','rebase')),
    commit_title TEXT,
    commit_message TEXT,
    author_email TEXT,
    enabled_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Job-level Actions concurrency. Groups share their names with workflow-level groups.
CREATE TABLE actions_job_concurrency (
    job_id BIGINT PRIMARY KEY REFERENCES actions_jobs(id) ON DELETE CASCADE,
    repo_id BIGINT NOT NULL,
    name_key TEXT NOT NULL,
    holding BOOLEAN NOT NULL DEFAULT FALSE,
    cancelled BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY(repo_id, name_key) REFERENCES actions_concurrency_groups(repo_id, name_key) ON DELETE CASCADE
);
CREATE INDEX actions_job_concurrency_group ON actions_job_concurrency(repo_id, name_key);
