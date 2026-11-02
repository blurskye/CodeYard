//! Collaborator invitations keep admission separate from permission grants.
use super::access::{self, AccessError};
use crate::backend::db::{self, models::AccessLevel, repositories::collaborators as records};
pub use records::{Added, Collaborator, CollaboratorQuery, Invitation};

#[derive(Debug)]
pub enum Error {
    Denied,
    NotFound,
    Invalid(String),
    Unavailable,
}

fn database_error(error: anyhow::Error) -> Error {
    match error.downcast::<records::Refusal>() {
        Ok(records::Refusal::Denied) => Error::Denied,
        Ok(records::Refusal::NotFound) => Error::NotFound,
        Ok(error) => Error::Invalid(error.to_string()),
        Err(_) => Error::Unavailable,
    }
}
fn access_error(error: AccessError) -> Error {
    match error {
        AccessError::NotFound => Error::NotFound,
        AccessError::Unavailable => Error::Unavailable,
        _ => Error::Denied,
    }
}

pub fn permission(value: &str) -> Option<AccessLevel> {
    match value {
        "read" | "pull" => Some(AccessLevel::Read),
        "triage" => Some(AccessLevel::Triage),
        "write" | "push" => Some(AccessLevel::Write),
        "maintain" => Some(AccessLevel::Maintain),
        "admin" => Some(AccessLevel::Admin),
        _ => None,
    }
}

pub async fn list(
    pool: &db::DbPool,
    actor: i64,
    repo_id: i64,
    query: &CollaboratorQuery<'_>,
) -> Result<Vec<Collaborator>, Error> {
    // The roster shows each person's permission, and an organization's members, so it needs
    // write-level access and, for an organization repository, membership of that organization.
    // Reading it changes nothing, so an archived or migration-locked repository still shows it.
    let repository = access::check_read(pool, repo_id, Some(actor))
        .await
        .map_err(access_error)?;
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| Error::Unavailable)?
        .ok_or(Error::NotFound)?;
    let writer = !user.is_banned
        && ((repository.owner_kind == "user"
            && (user.is_admin || repository.is_personal_owner(user.id)))
            || db::repositories::access::get_user_repo_access(pool, user.id, repository.id)
                .await
                .map_err(|_| Error::Unavailable)?
                .can_write());
    if !writer {
        return Err(Error::Denied);
    }
    if let Some(org) = repository.owner_org_id {
        let member = matches!(
            db::organizations::get_organization_role(pool, actor, org)
                .await
                .map_err(|_| Error::Unavailable)?,
            db::organizations::OrganizationOutcome::Applied(_)
        );
        if !member {
            return Err(Error::Denied);
        }
    }
    records::list(pool, repo_id, query)
        .await
        .map_err(database_error)
}
pub async fn add(
    pool: &db::DbPool,
    actor: i64,
    repo_id: i64,
    username: &str,
    permission: AccessLevel,
) -> Result<Added, Error> {
    records::add(pool, actor, repo_id, username, permission)
        .await
        .map_err(database_error)
}
pub async fn remove(
    pool: &db::DbPool,
    actor: i64,
    repo_id: i64,
    username: &str,
) -> Result<(), Error> {
    records::remove(pool, actor, repo_id, username)
        .await
        .map_err(database_error)
}
pub async fn invitations(
    pool: &db::DbPool,
    actor: i64,
    repo_id: Option<i64>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Invitation>, Error> {
    if let Some(repo) = repo_id {
        access::check_admin(pool, repo, actor)
            .await
            .map_err(access_error)?;
    }
    records::invitations(pool, actor, repo_id, offset, limit)
        .await
        .map_err(database_error)
}
pub async fn update_invitation(
    pool: &db::DbPool,
    actor: i64,
    repo_id: i64,
    id: i64,
    permission: Option<AccessLevel>,
) -> Result<Invitation, Error> {
    records::update_invitation(pool, actor, repo_id, id, permission)
        .await
        .map_err(database_error)
}
pub async fn respond(pool: &db::DbPool, actor: i64, id: i64, accept: bool) -> Result<(), Error> {
    records::respond(pool, actor, id, accept)
        .await
        .map_err(database_error)
}
