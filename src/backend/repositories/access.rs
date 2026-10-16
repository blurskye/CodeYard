//! Current repository read, write and administration decisions.
use crate::backend::db::{
    self,
    models::{Repository, User},
};
use anyhow::Result;

pub async fn can_read(
    pool: &db::DbPool,
    user: Option<&User>,
    repository: &Repository,
) -> Result<bool> {
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

pub async fn can_admin(pool: &db::DbPool, user: &User, repository: &Repository) -> Result<bool> {
    if user.must_change_password {
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
    if repository.is_archived || user.is_banned || user.must_change_password {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessError {
    NotFound,
    PasswordChangeRequired,
    WriteDenied,
    InvalidCollaboratorOwner,
    Unavailable,
}

pub(crate) enum CollaboratorChange {
    Applied(Box<User>),
    NotFound,
    PasswordChangeRequired,
    InvalidCollaboratorOwner,
}

pub(crate) trait CollaboratorAccessTransaction {
    async fn change_personal_repository_collaborator(
        &self,
        actor: i64,
        repo_id: i64,
        username: &str,
        permission: Option<db::models::AccessLevel>,
    ) -> Result<CollaboratorChange>;
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

pub async fn check_read(
    pool: &db::DbPool,
    repo_id: i64,
    actor: Option<i64>,
) -> std::result::Result<Repository, AccessError> {
    let repository = current_repository(pool, repo_id).await?;
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
    let change = match pool {
        db::DbPool::Postgres(native) => {
            native
                .change_personal_repository_collaborator(actor, repo_id, username, permission)
                .await
        }
        db::DbPool::Sqlite(native) => {
            native
                .change_personal_repository_collaborator(actor, repo_id, username, permission)
                .await
        }
    }
    .map_err(|_| AccessError::Unavailable)?;
    match change {
        CollaboratorChange::Applied(user) => Ok(*user),
        CollaboratorChange::NotFound => Err(AccessError::NotFound),
        CollaboratorChange::PasswordChangeRequired => Err(AccessError::PasswordChangeRequired),
        CollaboratorChange::InvalidCollaboratorOwner => Err(AccessError::InvalidCollaboratorOwner),
    }
}
