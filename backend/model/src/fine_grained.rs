use serde_json::{Map, Value};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Repository,
    Organization,
    Account,
}

pub struct Permission {
    pub key: &'static str,
    pub name: &'static str,
    pub group: Group,
    pub levels: &'static [&'static str],
}

const RW: &[&str] = &["read", "write"];

const RWA: &[&str] = &["read", "write", "admin"];

const fn permission(
    key: &'static str,
    name: &'static str,
    group: Group,
    levels: &'static [&'static str],
) -> Permission {
    Permission {
        key,
        name,
        group,
        levels,
    }
}

/// GitHub's permission names, limited to the features Codeyard has.
pub const PERMISSIONS: &[Permission] = &[
    permission("actions", "Actions", Group::Repository, RW),
    permission("administration", "Administration", Group::Repository, RW),
    permission("attestations", "Attestations", Group::Repository, RW),
    permission("checks", "Checks", Group::Repository, RW),
    permission("contents", "Contents", Group::Repository, RW),
    permission(
        "dependabot_secrets",
        "Dependabot secrets",
        Group::Repository,
        RW,
    ),
    permission("deployments", "Deployments", Group::Repository, RW),
    permission("discussions", "Discussions", Group::Repository, RW),
    permission("environments", "Environments", Group::Repository, RW),
    permission("issues", "Issues", Group::Repository, RW),
    permission("metadata", "Metadata", Group::Repository, &["read"]),
    permission("packages", "Packages", Group::Repository, RW),
    permission("pages", "Pages", Group::Repository, RW),
    permission("pull_requests", "Pull requests", Group::Repository, RW),
    permission("repository_hooks", "Webhooks", Group::Repository, RW),
    permission(
        "secret_scanning_alerts",
        "Secret scanning alerts",
        Group::Repository,
        RW,
    ),
    permission("secrets", "Secrets", Group::Repository, RW),
    permission(
        "security_events",
        "Code scanning alerts",
        Group::Repository,
        RW,
    ),
    permission("statuses", "Commit statuses", Group::Repository, RW),
    permission("variables", "Variables", Group::Repository, RW),
    permission(
        "vulnerability_alerts",
        "Dependabot alerts",
        Group::Repository,
        RW,
    ),
    permission("workflows", "Workflows", Group::Repository, &["write"]),
    permission("members", "Members", Group::Organization, RW),
    permission(
        "organization_administration",
        "Administration",
        Group::Organization,
        RW,
    ),
    permission(
        "organization_actions_variables",
        "Variables",
        Group::Organization,
        RW,
    ),
    permission(
        "organization_custom_properties",
        "Custom properties",
        Group::Organization,
        RWA,
    ),
    permission("organization_hooks", "Webhooks", Group::Organization, RW),
    permission(
        "organization_projects",
        "Projects",
        Group::Organization,
        RWA,
    ),
    permission("organization_secrets", "Secrets", Group::Organization, RW),
    permission(
        "organization_self_hosted_runners",
        "Self-hosted runners",
        Group::Organization,
        RW,
    ),
    permission(
        "organization_user_blocking",
        "Blocking users",
        Group::Organization,
        RW,
    ),
    permission("blocking", "Block another user", Group::Account, RW),
    permission("email_addresses", "Email addresses", Group::Account, RW),
    permission("followers", "Followers", Group::Account, RW),
    permission("gists", "Gists", Group::Account, &["write"]),
    permission("git_ssh_keys", "Git SSH keys", Group::Account, RW),
    permission("gpg_keys", "GPG keys", Group::Account, RW),
    permission(
        "interaction_limits",
        "Interaction limits",
        Group::Account,
        RW,
    ),
    permission("profile", "Profile", Group::Account, &["write"]),
    permission("ssh_signing_keys", "SSH signing keys", Group::Account, RW),
    permission("starring", "Starring", Group::Account, RW),
    permission("watching", "Watching", Group::Account, RW),
];

pub fn catalog(key: &str) -> Option<&'static Permission> {
    PERMISSIONS.iter().find(|permission| permission.key == key)
}

/// Whether `level` covers `write` (or read). `admin` covers both.
pub fn allows(permissions: &Map<String, Value>, key: &str, write: bool) -> bool {
    permissions
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|level| matches!(level, "write" | "admin") || (!write && level == "read"))
}
