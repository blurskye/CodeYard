//! Collaborator invitations keep admission separate from permission grants.

use codeyard_identity::repository_access::AccessError;

use codeyard_db_core::models::AccessLevel;
pub use codeyard_db_repos::repositories::collaborators::{
    Added, Collaborator, CollaboratorQuery, Invitation, InvitationSummary,
};

#[derive(Debug)]
pub enum Error {
    Denied,
    NotFound,
    Invalid(String),
    Unavailable,
}

fn database_error(error: anyhow::Error) -> Error {
    match error.downcast::<codeyard_db_repos::repositories::collaborators::Refusal>() {
        Ok(codeyard_db_repos::repositories::collaborators::Refusal::Denied) => Error::Denied,
        Ok(codeyard_db_repos::repositories::collaborators::Refusal::NotFound) => Error::NotFound,
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
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo_id: i64,
    query: &CollaboratorQuery<'_>,
) -> Result<Vec<Collaborator>, Error> {
    // The roster shows each person's permission, and an organization's members, so it needs
    // write-level access and, for an organization repository, membership of that organization.
    // Reading it changes nothing, so an archived or migration-locked repository still shows it.
    let repository = codeyard_identity::repository_access::check_read(pool, repo_id, Some(actor))
        .await
        .map_err(access_error)?;
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| Error::Unavailable)?
        .ok_or(Error::NotFound)?;
    let writer = !user.is_banned
        && ((repository.owner_kind == "user"
            && (user.is_admin || repository.is_personal_owner(user.id)))
            || codeyard_db_repos::repositories::access::get_user_repo_access(
                pool,
                user.id,
                repository.id,
            )
            .await
            .map_err(|_| Error::Unavailable)?
            .can_write());
    if !writer {
        return Err(Error::Denied);
    }
    if let Some(org) = repository.owner_org_id {
        let member = matches!(
            codeyard_db_orgs::organizations::get_organization_role(pool, actor, org)
                .await
                .map_err(|_| Error::Unavailable)?,
            codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(_)
        );
        if !member {
            return Err(Error::Denied);
        }
    }
    codeyard_db_repos::repositories::collaborators::list(pool, repo_id, query)
        .await
        .map_err(database_error)
}
pub async fn add(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo_id: i64,
    username: &str,
    permission: AccessLevel,
) -> Result<Added, Error> {
    codeyard_db_repos::repositories::collaborators::add(pool, actor, repo_id, username, permission)
        .await
        .map_err(database_error)
}
pub async fn remove(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo_id: i64,
    username: &str,
) -> Result<(), Error> {
    codeyard_db_repos::repositories::collaborators::remove(pool, actor, repo_id, username)
        .await
        .map_err(database_error)
}
pub async fn invitations(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo_id: Option<i64>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Invitation>, Error> {
    if let Some(repo) = repo_id {
        codeyard_identity::repository_access::check_admin(pool, repo, actor)
            .await
            .map_err(access_error)?;
    }
    codeyard_db_repos::repositories::collaborators::invitations(pool, actor, repo_id, offset, limit)
        .await
        .map_err(database_error)
}
pub async fn recipient_invitations(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    session: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<InvitationSummary>, Error> {
    codeyard_db_repos::repositories::collaborators::recipient_invitations(
        pool, actor, session, offset, limit,
    )
    .await
    .map_err(database_error)
}

pub async fn update_invitation(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo_id: i64,
    id: i64,
    permission: Option<AccessLevel>,
) -> Result<Invitation, Error> {
    codeyard_db_repos::repositories::collaborators::update_invitation(
        pool, actor, repo_id, id, permission,
    )
    .await
    .map_err(database_error)
}
pub async fn respond(
    pool: &codeyard_db_core::DbPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    id: i64,
    accept: bool,
) -> Result<(), Error> {
    codeyard_db_repos::repositories::collaborators::respond(pool, actor, id, accept)
        .await
        .map_err(database_error)
}
