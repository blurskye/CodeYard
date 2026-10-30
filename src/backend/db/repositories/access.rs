//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{AccessLevel, RepoAccess, Repository, User};
use crate::backend::db::DbPool;
use anyhow::Result;

/// Credential selection applied in SQL before a collection's count and page window.
/// Native repository visibility remains an additional, independent condition.
#[derive(Clone)]
pub struct ReadFilter {
    pub selection: ReadSelection,
    pub public_fallback: bool,
    pub classic: bool,
}

#[derive(Clone)]
pub enum ReadSelection {
    All,
    None,
    Repository(i64),
    Personal { token: i64, user: Option<i64>, org: Option<i64>, all: bool, permitted: bool },
    App { app: i64, installation: Option<i64>, repositories: Vec<i64> },
}

impl ReadFilter {
    /// All interpolated values are typed server-owned IDs or fixed SQL, never request text.
    pub(crate) fn condition(&self) -> String {
        let selection = match &self.selection {
            ReadSelection::All => "TRUE".to_owned(),
            ReadSelection::None => "FALSE".to_owned(),
            ReadSelection::Repository(repo) => format!("r.id={repo}"),
            ReadSelection::Personal { token, user, org, all, .. } => {
                let owner = match (user, org) {
                    (Some(user), None) => format!("r.owner_kind='user' AND r.owner_user_id={user}"),
                    (None, Some(org)) => format!("r.owner_kind='org' AND r.owner_org_id={org}"),
                    _ => "FALSE".to_owned(),
                };
                let selection = if *all { "TRUE".to_owned() } else {
                    format!("EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id={token} AND s.repo_id=r.id)")
                };
                format!("({owner}) AND ({selection})")
            }
            ReadSelection::App { app, installation, repositories } => {
                let installation = installation.map_or_else(||
                    "(i.user_id=u.id OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=i.org_id AND m.user_id=u.id) OR EXISTS(SELECT 1 FROM repo_access ra JOIN repositories ar ON ar.id=ra.repo_id WHERE ra.user_id=u.id AND ra.access_level BETWEEN 1 AND 5 AND (ar.owner_user_id=i.user_id OR ar.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories s WHERE s.installation_id=i.id AND s.repo_id=ar.id))))".to_owned(),
                    |id| format!("i.id={id}"));
                let selected = if repositories.is_empty() { "TRUE".to_owned() } else {
                    format!("r.id IN ({})", repositories.iter().map(i64::to_string).collect::<Vec<_>>().join(","))
                };
                format!("({selected}) AND EXISTS(SELECT 1 FROM github_app_installations i WHERE i.app_id={app} AND i.suspended_at IS NULL AND ({installation}) AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories s WHERE s.installation_id=i.id AND s.repo_id=r.id)))")
            }
        };
        let visibility = if matches!(&self.selection, ReadSelection::Personal { permitted: false, .. }) {
            // Selected repositories do not gain a missing family through public fallback.
            format!("NOT r.is_private AND NOT ({selection})")
        } else if self.public_fallback {
            format!("NOT r.is_private OR ({selection})")
        } else { selection };
        let policy = if self.classic {
            "NOT EXISTS(SELECT 1 FROM organization_token_policies p WHERE p.org_id=r.owner_org_id AND NOT p.classic_allowed)"
        } else { "TRUE" };
        format!("({visibility}) AND ({policy})")
    }
}

pub enum CollaboratorChange {
    Applied(Box<User>),
    NotFound,
    PasswordChangeRequired,
    InvalidCollaboratorOwner,
}

native_operation!(crate::backend::db::postgres::repositories::access::change_personal_repository_collaborator,
    crate::backend::db::sqlite::repositories::access::change_personal_repository_collaborator,
    fn change_personal_repository_collaborator(
    pool: &DbPool,
    actor: i64,
    repo_id: i64,
    username: &str,
    permission: Option<AccessLevel>,
) -> Result<CollaboratorChange>);

native_operation!(crate::backend::db::postgres::repositories::access::readable_repositories_by_ids,
    crate::backend::db::sqlite::repositories::access::readable_repositories_by_ids,
    fn readable_repositories_by_ids(pool: &DbPool, user_id: i64, ids: &[i64]) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::access::get_user_repo_access,
    crate::backend::db::sqlite::repositories::access::get_user_repo_access,
    fn get_user_repo_access(
    pool: &DbPool,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel>);

native_operation!(crate::backend::db::postgres::repositories::access::get_users_repo_access,
    crate::backend::db::sqlite::repositories::access::get_users_repo_access,
    fn get_users_repo_access(pool: &DbPool, user_ids: &[i64], repo_id: i64) -> Result<Vec<(i64, AccessLevel)>>);
native_operation!(crate::backend::db::postgres::repositories::access::readable_repository_ids,
    crate::backend::db::sqlite::repositories::access::readable_repository_ids,
    fn readable_repository_ids(pool: &DbPool, user_id: Option<i64>, repo_ids: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::backend::db::postgres::repositories::access::grant_repo_access,
    crate::backend::db::sqlite::repositories::access::grant_repo_access,
    fn grant_repo_access(
    pool: &DbPool,
    user_id: i64,
    repo_id: i64,
    level: AccessLevel,
) -> Result<RepoAccess>);

native_operation!(crate::backend::db::postgres::repositories::access::list_repo_collaborators,
    crate::backend::db::sqlite::repositories::access::list_repo_collaborators,
    fn list_repo_collaborators(pool: &DbPool, repo_id: i64) -> Result<Vec<RepoAccess>>);

native_operation!(crate::backend::db::postgres::repositories::access::write_access_user_ids,
    crate::backend::db::sqlite::repositories::access::write_access_user_ids,
    fn write_access_user_ids(pool: &DbPool, repo_id: i64, ids: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::backend::db::postgres::repositories::access::list_write_access_users,
    crate::backend::db::sqlite::repositories::access::list_write_access_users,
    fn list_write_access_users(pool: &DbPool, repo_id: i64, viewer_id: i64) -> Result<Vec<(String, AccessLevel)>>);

native_operation!(crate::backend::db::postgres::repositories::access::revoke_repo_access,
    crate::backend::db::sqlite::repositories::access::revoke_repo_access,
    fn revoke_repo_access(pool: &DbPool, user_id: i64, repo_id: i64) -> Result<bool>);
