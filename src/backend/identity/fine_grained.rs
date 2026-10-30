//! Fine-grained personal access tokens. Each token has one resource owner (the user or an
//! organization the user belongs to), a repository selection and named permissions. What it
//! can do is always the intersection of those grants with the user's own live rights: every
//! request still runs as the user, and this module only narrows it.
use super::tokens::{random_token, token_hash};
use crate::backend::db::{
    self,
    fine_grained_tokens::{within, Bounds, NewToken, Policy},
    models::Repository,
    DbPool,
};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

pub const PREFIX: &str = "cy_fgpat_";
/// The longest lifetime any token may have, and the default organization maximum.
pub const MAX_DAYS: i64 = 366;
pub const DEFAULT_DAYS: i64 = 30;
pub const MAX_REPOSITORIES: usize = db::fine_grained_tokens::MAX_REPOSITORIES;

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

/// What any token may read on a public repository, as an anonymous visitor can.
fn public_read() -> Value {
    let mut map = Map::new();
    for key in [
        "metadata",
        "contents",
        "issues",
        "pull_requests",
        "discussions",
        "actions",
        "checks",
        "statuses",
        "deployments",
        "pages",
    ] {
        map.insert(key.into(), Value::String("read".into()));
    }
    Value::Object(map)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    User(i64),
    Organization(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read-only access to public repositories, as everyone has.
    Public,
    All,
    Selected,
}

impl Access {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "public" => Some(Self::Public),
            "all" => Some(Self::All),
            "selected" => Some(Self::Selected),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::All => "all",
            Self::Selected => "selected",
        }
    }
    /// GitHub's name in organization grant listings.
    pub fn selection(self) -> &'static str {
        match self {
            Self::Public => "none",
            Self::All => "all",
            Self::Selected => "subset",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Account or organization is unavailable")]
    Denied,
    #[error("Token not found")]
    Missing,
    #[error("Token storage is unavailable")]
    Unavailable,
}

pub struct Request {
    pub name: String,
    pub description: String,
    pub owner: Owner,
    pub access: Access,
    pub repositories: Vec<i64>,
    pub permissions: Map<String, Value>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// The longest lifetime in days the owner allows; `None` allows tokens without expiry.
pub fn max_lifetime(owner: Owner, policy: Option<&Policy>) -> Option<i64> {
    match (owner, policy) {
        (Owner::Organization(_), Some(policy)) => policy.max_lifetime_days.map(i64::from),
        _ => Some(MAX_DAYS),
    }
}

/// Checks a request and returns its stored permissions, with metadata read always implied.
pub fn validate(
    request: &Request,
    policy: Option<&Policy>,
    now: DateTime<Utc>,
) -> Result<Map<String, Value>, TokenError> {
    let name = request.name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return Err(TokenError::Invalid("Use a name of 1-100 characters."));
    }
    if request.description.len() > 1024 || request.description.contains('\0') {
        return Err(TokenError::Invalid(
            "Use a description of at most 1024 characters.",
        ));
    }
    if request.permissions.len() > PERMISSIONS.len() {
        return Err(TokenError::Invalid("Choose each permission once."));
    }
    let mut permissions = Map::new();
    for (key, level) in &request.permissions {
        let Some(permission) = catalog(key) else {
            return Err(TokenError::Invalid("Choose only listed permissions."));
        };
        let Some(level) = level.as_str() else {
            return Err(TokenError::Invalid("Choose a listed access level."));
        };
        if level == "none" {
            continue;
        }
        if !permission.levels.contains(&level) {
            return Err(TokenError::Invalid("Choose a listed access level."));
        }
        let fits = match permission.group {
            Group::Repository => request.access != Access::Public || key == "metadata",
            Group::Organization => matches!(request.owner, Owner::Organization(_)),
            Group::Account => matches!(request.owner, Owner::User(_)),
        };
        if !fits {
            return Err(TokenError::Invalid(match permission.group {
                Group::Repository => "Repository permissions need all or selected repositories.",
                Group::Organization => "Organization permissions need an organization owner.",
                Group::Account => "Account permissions need your account as owner.",
            }));
        }
        permissions.insert(key.clone(), Value::String(level.to_owned()));
    }
    permissions.insert("metadata".into(), Value::String("read".into()));
    let mut unique = request.repositories.clone();
    unique.sort_unstable();
    unique.dedup();
    let repositories_fit = match request.access {
        Access::Selected => {
            !unique.is_empty()
                && unique.len() == request.repositories.len()
                && unique.len() <= MAX_REPOSITORIES
        }
        Access::Public | Access::All => request.repositories.is_empty(),
    };
    if !repositories_fit {
        return Err(TokenError::Invalid(
            "Select 1-100 repositories, or choose public or all repositories.",
        ));
    }
    if request.expires_at.is_some_and(|expiry| expiry <= now)
        || !within(request.expires_at, now, max_lifetime(request.owner, policy))
    {
        return Err(TokenError::Invalid(
            "Choose a future expiry within the owner's maximum lifetime.",
        ));
    }
    Ok(permissions)
}

pub struct Created {
    pub id: i64,
    pub value: String,
    pub status: String,
}

/// Creates a token while `session` is a live session of `user`. Selected repositories must
/// belong to the owner and be readable by the user.
pub async fn create(
    pool: &DbPool,
    user: i64,
    session: Option<&str>,
    request: Request,
) -> Result<Created, TokenError> {
    let session = session.ok_or(TokenError::Denied)?;
    let unavailable = |_| TokenError::Unavailable;
    let account = db::users::accounts::get_user_by_id(pool, user)
        .await
        .map_err(unavailable)?
        .ok_or(TokenError::Denied)?;
    let policy = match request.owner {
        Owner::User(id) if id == user => None,
        Owner::User(_) => return Err(TokenError::Denied),
        Owner::Organization(org) => {
            let policy = db::fine_grained_tokens::policy(pool, org)
                .await
                .map_err(unavailable)?;
            if !policy.fine_grained_allowed {
                return Err(TokenError::Invalid(
                    "This organization does not allow fine-grained tokens.",
                ));
            }
            Some(policy)
        }
    };
    let permissions = validate(&request, policy.as_ref(), Utc::now())?;
    for id in &request.repositories {
        let repo = db::repositories::records::get_repository_by_id(pool, *id)
            .await
            .map_err(unavailable)?;
        let readable = match &repo {
            Some(repo) if owned_by(request.owner, repo) => {
                crate::backend::repositories::access::can_read(pool, Some(&account), repo)
                    .await
                    .map_err(unavailable)?
            }
            _ => false,
        };
        if !readable {
            return Err(TokenError::Invalid(
                "Select only repositories of the resource owner that you can read.",
            ));
        }
    }
    let value = format!("{PREFIX}{}", random_token());
    let write = permissions.values().any(|level| level != "read");
    let permissions = Value::Object(permissions).to_string();
    let (id, status) = db::fine_grained_tokens::create(
        pool,
        user,
        session,
        &token_hash(&value),
        &NewToken {
            name: request.name.trim(),
            description: request.description.trim(),
            owner_org_id: match request.owner {
                Owner::User(_) => None,
                Owner::Organization(org) => Some(org),
            },
            repository_access: request.access.as_str(),
            permissions: &permissions,
            repositories: &request.repositories,
            scope: if write { "write" } else { "read" },
            expires_at: request.expires_at,
        },
    )
    .await
    .map_err(unavailable)?
    .ok_or(TokenError::Denied)?;
    Ok(Created { id, value, status })
}

/// Replaces a token's value and expiry. Its permissions, selection and approval stay.
pub async fn regenerate(
    pool: &DbPool,
    user: i64,
    session: Option<&str>,
    id: i64,
    expires_at: Option<DateTime<Utc>>,
) -> Result<String, TokenError> {
    let session = session.ok_or(TokenError::Denied)?;
    let unavailable = |_| TokenError::Unavailable;
    let token = db::fine_grained_tokens::get(pool, id)
        .await
        .map_err(unavailable)?
        .filter(|token| token.user_id == user)
        .ok_or(TokenError::Missing)?;
    let policy = match token.owner_org_id {
        Some(org) => Some(
            db::fine_grained_tokens::policy(pool, org)
                .await
                .map_err(unavailable)?,
        ),
        None => None,
    };
    let owner = token
        .owner_org_id
        .map_or(Owner::User(user), Owner::Organization);
    let now = Utc::now();
    if expires_at.is_some_and(|expiry| expiry <= now)
        || !within(expires_at, now, max_lifetime(owner, policy.as_ref()))
    {
        return Err(TokenError::Invalid(
            "Choose a future expiry within the owner's maximum lifetime.",
        ));
    }
    let value = format!("{PREFIX}{}", random_token());
    if db::fine_grained_tokens::regenerate(pool, user, session, id, &token_hash(&value), expires_at)
        .await
        .map_err(unavailable)?
    {
        Ok(value)
    } else {
        Err(TokenError::Denied)
    }
}

fn owned_by(owner: Owner, repo: &Repository) -> bool {
    match owner {
        Owner::User(id) => repo.owner_kind == "user" && repo.owner_user_id == Some(id),
        Owner::Organization(id) => repo.owner_kind == "org" && repo.owner_org_id == Some(id),
    }
}

/// A loaded token: its bounds and whether its owner still lets it act.
#[derive(Clone, Debug)]
pub struct Grant {
    pub token_id: i64,
    pub owner: Owner,
    pub access: Access,
    pub permissions: Map<String, Value>,
    /// False while the organization has not approved it, after the user left the
    /// organization, or when the organization's policy no longer allows it. An inactive
    /// token reads public data only.
    pub active: bool,
}

impl Grant {
    pub fn from_bounds(bounds: Bounds) -> anyhow::Result<Self> {
        let permissions = match serde_json::from_str(&bounds.permissions)? {
            Value::Object(map) => map,
            _ => anyhow::bail!("fine-grained permissions are not an object"),
        };
        let owner = bounds
            .owner_org_id
            .map_or(Owner::User(bounds.user_id), Owner::Organization);
        let active = bounds.active();
        Ok(Self {
            token_id: bounds.token_id,
            owner,
            access: Access::parse(&bounds.repository_access)
                .ok_or_else(|| anyhow::anyhow!("unknown repository access"))?,
            permissions,
            active,
        })
    }

    /// The permissions in one group, with metadata read implied for repositories.
    pub fn group(&self, group: Group) -> Map<String, Value> {
        self.permissions
            .iter()
            .filter(|(key, _)| catalog(key).is_some_and(|permission| permission.group == group))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Whether the token is live and its repository selection covers `repo`. Other public
    /// repositories read only as they do anonymously.
    pub async fn reaches(
        &self,
        pool: &DbPool,
        repo: &Repository,
    ) -> anyhow::Result<bool> {
        Ok(self.active
            && owned_by(self.owner, repo)
            && match self.access {
                Access::Public => false,
                Access::All => true,
                Access::Selected => {
                    !repo.is_gist
                        && db::fine_grained_tokens::selected(pool, self.token_id, repo.id).await?
                }
            })
    }

    /// The token's permissions on one repository, before the user's own rights apply.
    pub async fn repository(&self, pool: &DbPool, repo: i64) -> anyhow::Result<Option<Value>> {
        let Some(repo) = db::repositories::records::get_repository_by_id(pool, repo).await? else {
            return Ok(None);
        };
        let reach = self.reaches(pool, &repo).await?;
        if repo.is_gist {
            // A gist is an account resource: its owner's gists permission covers it.
            if self.account(repo.owner_user_id.unwrap_or_default(), "gists", true) {
                return Ok(Some(
                    serde_json::json!({"metadata":"read","contents":"write"}),
                ));
            }
        } else if reach {
            let mut permissions = if repo.is_private {
                Map::new()
            } else {
                public_read().as_object().cloned().unwrap_or_default()
            };
            permissions.extend(self.group(Group::Repository));
            return Ok(Some(Value::Object(permissions)));
        }
        Ok((!repo.is_private).then(public_read))
    }

    /// Whether the token may create a repository for `owner`: only its own owner, with all
    /// repositories selected and administration write.
    pub fn creates_repository(&self, owner: Owner) -> bool {
        self.active
            && self.access == Access::All
            && self.owner == owner
            && allows(&self.permissions, "administration", true)
    }

    /// Whether the token acts for the account itself with `key`.
    pub fn account(&self, user: i64, key: &str, write: bool) -> bool {
        self.active && self.owner == Owner::User(user) && allows(&self.permissions, key, write)
    }

    /// Whether the token acts for organization `org` with `key`.
    pub fn organization(&self, org: i64, key: &str, write: bool) -> bool {
        self.active
            && self.owner == Owner::Organization(org)
            && allows(&self.permissions, key, write)
    }
}

pub async fn grant(pool: &DbPool, token: i64) -> anyhow::Result<Option<Grant>> {
    db::fine_grained_tokens::bounds(pool, token)
        .await?
        .map(Grant::from_bounds)
        .transpose()
}

/// What a REST request needs from a fine-grained token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// Answered as for an anonymous request: public data only.
    Public,
    /// Needs no permission: the API root, rate limits and the user's own profile.
    Free,
    Repository(&'static str),
    Account(&'static str),
    /// The organization named by the path's second segment.
    Organization(&'static str),
    /// A new repository for the token's owner: all repositories and administration write.
    CreateRepository,
    /// A fork or template copy: `key` read on the source repository. The handler checks the
    /// destination owner, named in the body, as for `CreateRepository`.
    CopyRepository(&'static str),
    /// The owner's packages: `/user/packages`, `/users/{user}/packages`, `/orgs/{org}/packages`.
    Packages,
    Denied,
}

impl Need {
    /// The permission named in `X-Accepted-GitHub-Permissions`.
    pub fn permission(self) -> Option<&'static str> {
        match self {
            Self::Repository(key) | Self::Account(key) | Self::Organization(key) => Some(key),
            Self::CreateRepository | Self::CopyRepository(_) => Some("administration"),
            Self::Packages => Some("packages"),
            Self::Public | Self::Free | Self::Denied => None,
        }
    }
}

pub fn need(path: &str, write: bool) -> Need {
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["markdown", ..] => Need::Free,
        [""]
        | ["meta" | "versions" | "zen" | "octocat" | "rate_limit" | "emojis" | "licenses"
        | "codes_of_conduct" | "gitignore", ..]
            if !write =>
        {
            Need::Free
        }
        ["user"] => {
            if write {
                Need::Account("profile")
            } else {
                Need::Free
            }
        }
        ["user", "emails" | "email" | "public_emails", ..] => Need::Account("email_addresses"),
        ["user", "followers"] | ["user", "following", ..] => Need::Account("followers"),
        ["user", "gpg_keys", ..] => Need::Account("gpg_keys"),
        ["user", "keys", ..] => Need::Account("git_ssh_keys"),
        ["user", "ssh_signing_keys", ..] => Need::Account("ssh_signing_keys"),
        ["user", "starred", ..] => Need::Account("starring"),
        ["user", "subscriptions"] | ["repos", _, _, "subscription"] => Need::Account("watching"),
        ["user", "blocks", ..] => Need::Account("blocking"),
        ["user", "interaction-limits"] => Need::Account("interaction_limits"),
        ["user", "social_accounts"] => {
            if write {
                Need::Account("profile")
            } else {
                Need::Free
            }
        }
        ["user", "repos"] if !write => Need::Free,
        ["user", "repos"] if write => Need::CreateRepository,
        ["user", "packages", ..] | ["users", _, "packages", ..] => Need::Packages,
        ["user", id] if !write && id.parse::<i64>().is_ok() => Need::Public,
        ["users", ..] if !write => Need::Public,
        ["orgs", _] => {
            if write {
                Need::Organization("organization_administration")
            } else {
                Need::Public
            }
        }
        ["orgs", _, rest @ ..] => organization_need(rest, write),
        ["repos", _, _, "forks"] if write => Need::CopyRepository("metadata"),
        ["repos", _, _, "generate"] if write => Need::CopyRepository("contents"),
        ["repos", _, _, ..] => {
            repository_permission(path, write).map_or(Need::Denied, Need::Repository)
        }
        ["repositories" | "search" | "events" | "networks" | "feeds" | "advisories"
        | "organizations", ..]
            if !write =>
        {
            Need::Public
        }
        ["gists"] | ["gists", "starred"] => Need::Account("gists"),
        ["gists", "public"] if !write => Need::Public,
        ["gists", _, "star"] => Need::Account("gists"),
        ["gists", ..] => {
            if write {
                Need::Account("gists")
            } else {
                Need::Public
            }
        }
        _ => Need::Denied,
    }
}

fn organization_need(rest: &[&str], write: bool) -> Need {
    match rest {
        ["members"
        | "memberships"
        | "invitations"
        | "failed_invitations"
        | "public_members"
        | "teams"
        | "outside_collaborators"
        | "organization-roles"
        | "security-managers", ..] => Need::Organization("members"),
        ["hooks", ..] => Need::Organization("organization_hooks"),
        ["actions", "secrets", ..] => Need::Organization("organization_secrets"),
        ["actions", "variables", ..] => Need::Organization("organization_actions_variables"),
        ["actions", "runners" | "runner-groups" | "hosted-runners", ..] => {
            Need::Organization("organization_self_hosted_runners")
        }
        ["actions", ..]
        | ["settings" | "interaction-limits" | "rulesets" | "issue-types" | "issue-fields"
        | "code-security" | "immutable-releases" | "private-registries", ..] => {
            Need::Organization("organization_administration")
        }
        ["blocks", ..] => Need::Organization("organization_user_blocking"),
        ["properties", ..] => Need::Organization("organization_custom_properties"),
        ["projectsV2", ..] => Need::Organization("organization_projects"),
        ["repos"] if write => Need::CreateRepository,
        // The list handler drops private repositories outside the token's selection.
        ["repos"] if !write => Need::Free,
        ["events"] if !write => Need::Public,
        ["packages", ..] => Need::Packages,
        // Fine-grained tokens cannot manage token grants, as on GitHub.
        _ => Need::Denied,
    }
}

/// The repository permission a request needs, extending the App mapping with the
/// permissions only personal tokens can hold.
pub fn repository_permission(path: &str, write: bool) -> Option<&'static str> {
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    if !matches!(parts.as_slice(), ["repos", _, _, ..]) {
        return None;
    }
    let read = |key| (!write).then_some(key);
    match (parts.get(3).copied(), parts.get(4).copied()) {
        (Some("actions"), Some("secrets" | "organization-secrets")) => Some("secrets"),
        (Some("actions"), Some("variables" | "organization-variables")) => Some("variables"),
        (Some("actions"), Some("runners" | "oidc" | "policies")) => Some("administration"),
        (Some("actions"), Some("concurrency_groups")) => Some("actions"),
        (Some("environments"), _) if !write || matches!(parts.get(5).copied(), Some("secrets" | "variables")) => Some("environments"),
        (Some("hooks"), _) => Some("repository_hooks"),
        (Some("pulls"), _)
            if write
                && matches!(
                    parts.get(5).copied(),
                    Some("merge" | "merge-async" | "update-branch")
                ) =>
        {
            Some("contents")
        }
        (Some("dependabot"), Some("alerts")) => Some("vulnerability_alerts"),
        (Some("dependabot"), Some("secrets")) => Some("dependabot_secrets"),
        (Some("secret-scanning"), Some("custom-patterns")) => Some("administration"),
        (Some("secret-scanning"), _) => Some("secret_scanning_alerts"),
        (Some("code-scanning"), _) => Some("security_events"),
        (Some("attestations"), _) => Some("attestations"),
        (Some("dispatches" | "merges" | "merge-upstream" | "license" | "codeowners"), _) => {
            Some("contents")
        }
        (Some("dependency-graph"), _) => Some("contents"),
        (
            Some(
                "keys"
                | "invitations"
                | "vulnerability-alerts"
                | "automated-security-fixes"
                | "autolinks"
                | "rulesets"
                | "interaction-limits"
                | "immutable-releases"
                | "private-vulnerability-reporting"
                | "transfer"
                | "traffic"
                | "code-security-configuration",
            ),
            _,
        ) => Some("administration"),
        (Some("topics" | "properties"), _) => {
            Some(if write { "administration" } else { "metadata" })
        }
        (
            Some(
                "stargazers" | "subscribers" | "forks" | "events" | "community" | "activity"
                | "hash-algorithm" | "rules" | "teams",
            ),
            _,
        ) => read("metadata"),
        (Some("issue-types"), _) => read("issues"),
        (Some("stacks"), _) => Some("pull_requests"),
        (Some("comments"), _) => Some("contents"),
        _ => super::github_apps::repository_permission(path, write),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;

    fn request(owner: Owner, access: Access, permissions: Value) -> Request {
        Request {
            name: "deploy".into(),
            description: String::new(),
            owner,
            access,
            repositories: if access == Access::Selected {
                vec![1]
            } else {
                vec![]
            },
            permissions: permissions.as_object().unwrap().clone(),
            expires_at: Some(Utc::now() + Duration::days(DEFAULT_DAYS)),
        }
    }

    #[test]
    fn fine_grained_permissions_follow_owner_and_selection() {
        let now = Utc::now();
        let user = Owner::User(1);
        let org = Owner::Organization(2);
        let stored = validate(
            &request(
                user,
                Access::All,
                json!({"contents":"read","issues":"none"}),
            ),
            None,
            now,
        )
        .unwrap();
        assert_eq!(
            Value::Object(stored),
            json!({"contents":"read","metadata":"read"})
        );
        for (owner, access, permissions) in [
            (user, Access::All, json!({"members":"read"})),
            (org, Access::All, json!({"email_addresses":"read"})),
            (user, Access::Public, json!({"contents":"read"})),
            (user, Access::All, json!({"contents":"admin"})),
            (user, Access::All, json!({"workflows":"read"})),
            (user, Access::All, json!({"unknown":"read"})),
            (user, Access::All, json!({"contents":true})),
        ] {
            assert!(validate(&request(owner, access, permissions), None, now).is_err());
        }
        let mut selected = request(user, Access::Selected, json!({}));
        selected.repositories = vec![];
        assert!(validate(&selected, None, now).is_err());
        selected.repositories = vec![3, 3];
        assert!(validate(&selected, None, now).is_err());
        selected.repositories = (0..=MAX_REPOSITORIES as i64).collect();
        assert!(validate(&selected, None, now).is_err());
        let mut all = request(user, Access::All, json!({}));
        all.repositories = vec![1];
        assert!(validate(&all, None, now).is_err());
    }

    #[test]
    fn fine_grained_expiry_follows_the_owner_policy() {
        let now = Utc::now();
        let mut token = request(Owner::User(1), Access::All, json!({}));
        token.expires_at = None;
        assert!(validate(&token, None, now).is_err(), "a user token expires");
        token.expires_at = Some(now + Duration::days(MAX_DAYS));
        assert!(validate(&token, None, now).is_ok());
        token.expires_at = Some(now + Duration::days(MAX_DAYS + 2));
        assert!(validate(&token, None, now).is_err());
        token.expires_at = Some(now - Duration::seconds(1));
        assert!(validate(&token, None, now).is_err());

        let mut token = request(Owner::Organization(2), Access::All, json!({}));
        let mut policy = Policy {
            max_lifetime_days: Some(7),
            ..Policy::default()
        };
        token.expires_at = Some(now + Duration::days(30));
        assert!(validate(&token, Some(&policy), now).is_err());
        token.expires_at = Some(now + Duration::days(7));
        assert!(validate(&token, Some(&policy), now).is_ok());
        token.expires_at = None;
        assert!(validate(&token, Some(&policy), now).is_err());
        policy.max_lifetime_days = None;
        assert!(validate(&token, Some(&policy), now).is_ok());
    }

    fn bounds(owner_org_id: Option<i64>, status: &str, member: bool) -> Bounds {
        let now = Utc::now();
        Bounds {
            token_id: 1,
            user_id: 10,
            owner_org_id,
            repository_access: "all".into(),
            permissions: json!({"metadata":"read","contents":"write","members":"read"}).to_string(),
            status: status.into(),
            expires_at: Some(now + Duration::days(30)),
            requested_at: now,
            member,
            fine_grained_allowed: true,
            max_lifetime_days: Some(366),
        }
    }

    #[test]
    fn fine_grained_grants_act_only_while_the_organization_allows_them() {
        assert!(
            Grant::from_bounds(bounds(None, "approved", true))
                .unwrap()
                .active
        );
        assert!(
            Grant::from_bounds(bounds(Some(2), "approved", true))
                .unwrap()
                .active
        );
        for (status, member) in [
            ("pending", true),
            ("denied", true),
            ("revoked", true),
            ("approved", false),
        ] {
            assert!(
                !Grant::from_bounds(bounds(Some(2), status, member))
                    .unwrap()
                    .active
            );
        }
        let mut lowered = bounds(Some(2), "approved", true);
        lowered.max_lifetime_days = Some(7);
        assert!(!Grant::from_bounds(lowered).unwrap().active);
        let mut refused = bounds(Some(2), "approved", true);
        refused.fine_grained_allowed = false;
        assert!(!Grant::from_bounds(refused).unwrap().active);

        let grant = Grant::from_bounds(bounds(Some(2), "approved", true)).unwrap();
        assert!(grant.organization(2, "members", false));
        assert!(!grant.organization(2, "members", true));
        assert!(!grant.organization(3, "members", false));
        assert!(!grant.account(10, "members", false));
        assert_eq!(
            Value::Object(grant.group(Group::Repository)),
            json!({"metadata":"read","contents":"write"})
        );
    }

    #[test]
    fn fine_grained_routes_name_one_permission() {
        assert_eq!(need("/", false), Need::Free);
        assert_eq!(need("/user", false), Need::Free);
        assert_eq!(need("/user", true), Need::Account("profile"));
        assert_eq!(need("/user/keys/4", true), Need::Account("git_ssh_keys"));
        assert_eq!(need("/search/repositories", false), Need::Public);
        assert_eq!(need("/users/alice/repos", false), Need::Public);
        assert_eq!(need("/user/repos", false), Need::Free);
        assert_eq!(need("/orgs/acme/repos", false), Need::Free);
        assert_eq!(need("/user/repos", true), Need::CreateRepository);
        assert_eq!(
            need("/orgs/acme/hooks", false),
            Need::Organization("organization_hooks")
        );
        assert_eq!(
            need("/orgs/acme/personal-access-tokens", false),
            Need::Denied
        );
        assert_eq!(
            need("/orgs/acme/personal-access-token-requests", true),
            Need::Denied
        );
        assert_eq!(need("/notifications", false), Need::Denied);
        assert_eq!(need("/repos/a/b/issues", true), Need::Repository("issues"));
        assert_eq!(
            need("/repos/a/b/hooks", false),
            Need::Repository("repository_hooks")
        );
        assert_eq!(
            need("/repos/a/b/actions/secrets", true),
            Need::Repository("secrets")
        );
        assert_eq!(
            need("/repos/a/b/pulls/1/merge", true),
            Need::Repository("contents")
        );
        assert_eq!(need("/repos/a/b", true), Need::Repository("administration"));
        assert_eq!(need("/repos/a/b/forks", false), Need::Repository("metadata"));
        assert_eq!(need("/repos/a/b/forks", true), Need::CopyRepository("metadata"));
        assert_eq!(need("/repos/a/b/generate", true), Need::CopyRepository("contents"));
        assert_eq!(need("/repos/a/b/import", false), Need::Denied);
    }
}
