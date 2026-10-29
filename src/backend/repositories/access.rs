//! Current repository read, write and administration decisions.
use crate::backend::db::{
    self,
    models::{Repository, User},
    repositories::access::CollaboratorChange,
};
use anyhow::Result;

/// The access projection with the personal-repository administrator policy applied.
pub async fn user_access(
    pool: &db::DbPool,
    user: &User,
    repository: &Repository,
) -> Result<db::models::AccessLevel> {
    if repository.is_gist || user.is_banned || user.must_change_password {
        return Ok(db::models::AccessLevel::None);
    }
    if repository.owner_kind == "user" && user.is_admin {
        return Ok(db::models::AccessLevel::Owner);
    }
    db::repositories::access::get_user_repo_access(pool, user.id, repository.id).await
}

/// Gists use a distinct clone namespace and never grant ordinary repository access.
pub async fn user_git_access(
    pool: &db::DbPool,
    user: &User,
    repository: &Repository,
) -> Result<db::models::AccessLevel> {
    if !repository.is_gist {
        return user_access(pool, user, repository).await;
    }
    if user.is_banned || user.must_change_password {
        return Ok(db::models::AccessLevel::None);
    }
    let gist = db::gists::by_repository(pool, repository.id)
        .await?
        .filter(|gist| gist.ready && Some(gist.owner_id) == repository.owner_user_id);
    Ok(match gist {
        Some(gist) if gist.owner_id == user.id => db::models::AccessLevel::Owner,
        Some(_) => db::models::AccessLevel::Read,
        None => db::models::AccessLevel::None,
    })
}
pub async fn can_read_git(
    pool: &db::DbPool,
    user: Option<&User>,
    repository: &Repository,
) -> Result<bool> {
    if !repository.is_gist {
        return can_read(pool, user, repository).await;
    }
    match user {
        Some(user) => Ok(user_git_access(pool, user, repository).await?.can_read()),
        None => Ok(db::gists::by_repository(pool, repository.id)
            .await?
            .is_some_and(|gist| gist.ready)),
    }
}

pub async fn can_read(
    pool: &db::DbPool,
    user: Option<&User>,
    repository: &Repository,
) -> Result<bool> {
    if repository.is_gist {
        return Ok(false);
    }
    if !repository.is_private {
        return Ok(true);
    }
    let Some(user) = user else {
        return Ok(false);
    };
    if repository.owner_kind == "user" && (user.is_admin || repository.is_personal_owner(user.id)) {
        return Ok(true);
    }
    Ok(
        db::repositories::access::get_user_repo_access(pool, user.id, repository.id)
            .await?
            .can_read(),
    )
}

/// The author's current relationship to a repository, shared by API presentations.
pub async fn author_association(
    pool: &db::DbPool,
    repository: &Repository,
    user_id: i64,
) -> Result<&'static str> {
    if repository.is_personal_owner(user_id) {
        return Ok("OWNER");
    }
    let member = match repository.owner_org_id {
        Some(org) => matches!(
            db::organizations::membership::get_organization_role(pool, user_id, org).await?,
            db::organizations::OrganizationOutcome::Applied(_)
        ),
        None => false,
    };
    let writer = db::repositories::access::get_user_repo_access(pool, user_id, repository.id)
        .await?
        .can_write();
    Ok(if member {
        "MEMBER"
    } else if writer {
        "COLLABORATOR"
    } else {
        "NONE"
    })
}

pub async fn can_admin(pool: &db::DbPool, user: &User, repository: &Repository) -> Result<bool> {
    if repository.is_gist || user.is_banned || user.must_change_password {
        return Ok(false);
    }
    if repository.owner_kind == "user" && (user.is_admin || repository.is_personal_owner(user.id)) {
        return Ok(true);
    }
    Ok(
        db::repositories::access::get_user_repo_access(pool, user.id, repository.id)
            .await?
            .can_admin(),
    )
}

/// Write permission for code and metadata: owners and site admins of personal
/// repositories, plus collaborators/teams with write access.
pub async fn can_write(pool: &db::DbPool, user: &User, repository: &Repository) -> Result<bool> {
    if repository.is_gist
        || repository.is_archived
        || user.is_banned
        || user.must_change_password
        || db::repository_exports::locked(pool, repository.id).await?
    {
        return Ok(false);
    }
    if repository.owner_kind == "user" && (user.is_admin || repository.is_personal_owner(user.id)) {
        return Ok(true);
    }
    Ok(
        db::repositories::access::get_user_repo_access(pool, user.id, repository.id)
            .await?
            .can_write(),
    )
}

/// Triage sets labels, assignees and milestones on issues and pull requests; it cannot edit the repository.
pub async fn can_triage(pool: &db::DbPool, user: &User, repository: &Repository) -> Result<bool> {
    if repository.is_gist
        || repository.is_archived
        || user.is_banned
        || user.must_change_password
        || db::repository_exports::locked(pool, repository.id).await?
    {
        return Ok(false);
    }
    if repository.owner_kind == "user" && (user.is_admin || repository.is_personal_owner(user.id)) {
        return Ok(true);
    }
    Ok(
        db::repositories::access::get_user_repo_access(pool, user.id, repository.id)
            .await?
            .can_triage(),
    )
}

/// Moderation of other people's comments: maintainers, owners and site administrators of personal
/// repositories. Archived repositories take no moderation.
pub async fn can_maintain(pool: &db::DbPool, user: &User, repository: &Repository) -> Result<bool> {
    if repository.is_gist
        || repository.is_archived
        || user.is_banned
        || user.must_change_password
        || db::repository_exports::locked(pool, repository.id).await?
    {
        return Ok(false);
    }
    if repository.owner_kind == "user" && (user.is_admin || repository.is_personal_owner(user.id)) {
        return Ok(true);
    }
    Ok(
        db::repositories::access::get_user_repo_access(pool, user.id, repository.id)
            .await?
            .can_maintain(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessError {
    NotFound,
    PasswordChangeRequired,
    WriteDenied,
    InvalidCollaboratorOwner,
    Unavailable,
}

pub async fn current_repository(
    pool: &db::DbPool,
    id: i64,
) -> std::result::Result<Repository, AccessError> {
    db::repositories::records::get_repository_by_id(pool, id)
        .await
        .map_err(|_| AccessError::Unavailable)?
        .ok_or(AccessError::NotFound)
}

/// A repository that a caller already resolved and authorized, read again by ID so that a
/// rename in between cannot switch it, with its owner's current name for links.
pub async fn current_repository_with_owner(
    pool: &db::DbPool,
    id: i64,
) -> std::result::Result<(Repository, String), AccessError> {
    let repository = current_repository(pool, id).await?;
    let owner = db::repositories::records::repository_owner_name(pool, id)
        .await
        .map_err(|_| AccessError::Unavailable)?
        .ok_or(AccessError::NotFound)?;
    Ok((repository, owner))
}

pub async fn check_read(
    pool: &db::DbPool,
    repo_id: i64,
    actor: Option<i64>,
) -> std::result::Result<Repository, AccessError> {
    let repository = current_repository(pool, repo_id).await?;
    if repository.is_gist {
        return Err(AccessError::NotFound);
    }
    if !repository.is_private {
        return Ok(repository);
    }
    let id = actor.ok_or(AccessError::NotFound)?;
    let user = db::users::accounts::get_user_by_id(pool, id)
        .await
        .map_err(|_| AccessError::Unavailable)?
        .ok_or(AccessError::NotFound)?;
    let allowed = can_read(pool, Some(&user), &repository)
        .await
        .map_err(|_| AccessError::Unavailable)?;
    if allowed {
        Ok(repository)
    } else {
        Err(AccessError::NotFound)
    }
}

pub async fn check_admin(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
) -> std::result::Result<Repository, AccessError> {
    let repository = current_repository(pool, repo_id).await?;
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| AccessError::Unavailable)?
        .ok_or(AccessError::NotFound)?;
    if user.must_change_password {
        return Err(AccessError::PasswordChangeRequired);
    }
    let allowed = can_admin(pool, &user, &repository)
        .await
        .map_err(|_| AccessError::Unavailable)?;
    if allowed {
        Ok(repository)
    } else {
        Err(AccessError::NotFound)
    }
}

pub async fn check_write(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
) -> std::result::Result<(Repository, User), AccessError> {
    let repository = current_repository(pool, repo_id).await?;
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| AccessError::Unavailable)?
        .ok_or(AccessError::NotFound)?;
    if !can_read(pool, Some(&user), &repository)
        .await
        .map_err(|_| AccessError::Unavailable)?
    {
        return Err(AccessError::NotFound);
    }
    if can_write(pool, &user, &repository)
        .await
        .map_err(|_| AccessError::Unavailable)?
    {
        return Ok((repository, user));
    }
    if repository.is_archived || user.must_change_password {
        return Err(AccessError::WriteDenied);
    }
    Err(AccessError::NotFound)
}

/// Personal repository owners retain the existing collaborator policy.
pub async fn change_collaborator(
    pool: &db::DbPool,
    actor: i64,
    repo_id: i64,
    username: &str,
    permission: Option<db::models::AccessLevel>,
) -> std::result::Result<User, AccessError> {
    let change = db::repositories::access::change_personal_repository_collaborator(
        pool, actor, repo_id, username, permission,
    )
    .await
    .map_err(|_| AccessError::Unavailable)?;
    match change {
        CollaboratorChange::Applied(user) => Ok(*user),
        CollaboratorChange::NotFound => Err(AccessError::NotFound),
        CollaboratorChange::PasswordChangeRequired => Err(AccessError::PasswordChangeRequired),
        CollaboratorChange::InvalidCollaboratorOwner => Err(AccessError::InvalidCollaboratorOwner),
    }
}
