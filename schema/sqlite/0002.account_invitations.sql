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
