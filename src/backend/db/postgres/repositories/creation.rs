use crate::backend::db::models::Repository;
use crate::backend::db::{
    models::OrgRole,
    organizations::OrganizationOutcome,
    repositories::creation::{
        NewRepository, RepositoryCreationOwner, RepositoryStorageRefusal, RepositoryTarget,
    },
};
use anyhow::Result;
use sqlx::PgPool;

/// Whether the actor may create a repository for this owner; `Ok(true)` for site admins.
pub(crate) async fn creation_authority(
    connection: &mut sqlx::PgConnection,
    owner: RepositoryCreationOwner,
) -> Result<bool> {
    match owner.target {
        RepositoryTarget::Personal(user) if user == owner.actor_id => {
            sqlx::query_scalar("SELECT is_admin FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR SHARE")
                .bind(owner.actor_id).fetch_optional(connection).await?
                .ok_or_else(|| RepositoryStorageRefusal::Denied.into())
        }
        RepositoryTarget::Organization(org) => {
            match crate::backend::db::postgres::organizations::authority::authority(connection, owner.actor_id, org).await? {
                OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => Ok(false),
                _ => Err(RepositoryStorageRefusal::Denied.into()),
            }
        }
        _ => Err(RepositoryStorageRefusal::Denied.into()),
    }
}

async fn fork_source(
    connection: &mut sqlx::PgConnection,
    actor: i64,
    admin: bool,
    id: i64,
) -> Result<Repository> {
    // Lock the source row for update now: inserting the fork updates its fork
    // count, and two forks that each held a shared lock would deadlock on that.
    let source = sqlx::query_as::<_, Repository>(
        "SELECT * FROM repositories WHERE id=$1 AND NOT is_archived FOR NO KEY UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RepositoryStorageRefusal::Denied)?;
    let access = super::access::user_repo_access_in_transaction(connection, actor, id).await?;
    // Site admins may fork any personal repository, not organization ones.
    let personal_admin = admin
        && source.owner_kind == "user"
        && source.owner_user_id.is_some()
        && source.owner_org_id.is_none();
    if !access.can_read() && !personal_admin {
        return Err(RepositoryStorageRefusal::Denied.into());
    }
    Ok(source)
}

/// Inserts the repository row after checking the actor may create it here.
/// `None` means the name (or this user's fork of the source) already exists.
pub async fn insert_repository(
    pool: &PgPool,
    input: NewRepository<'_>,
) -> Result<Option<Repository>> {
    if matches!(input.owner.target, RepositoryTarget::Organization(_)) && input.source_id.is_some()
    {
        return Err(RepositoryStorageRefusal::UnsupportedOrganizationFork.into());
    }
    let mut tx = pool.begin().await?;
    let creator = input.owner.actor_id;
    let admin = creation_authority(&mut tx, input.owner).await?;
    let (kind, user, org) = match input.owner.target {
        RepositoryTarget::Personal(id) => ("user", Some(id), None),
        RepositoryTarget::Organization(id) => ("org", None, Some(id)),
    };
    // Lock the source first; a competing fork of it then waits here and sees this one.
    let source = match input.source_id {
        Some(id) => Some(fork_source(&mut tx, input.owner.actor_id, admin, id).await?),
        None => None,
    };
    if source
        .as_ref()
        .is_some_and(|source| source.storage_node_id != input.storage_node_id)
    {
        return Err(RepositoryStorageRefusal::StalePlacement.into());
    }
    if let Some(source_id) = input.source_id {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE owner_kind='user' AND owner_user_id=$1 AND owner_org_id IS NULL AND forked_from_id=$2)")
            .bind(user)
            .bind(source_id)
            .fetch_one(&mut *tx)
            .await?;
        if exists {
            return Ok(None);
        }
    }
    let private = input.is_private || source.as_ref().is_some_and(|repo| repo.is_private);
    let repository = sqlx::query_as::<_, Repository>(
        "INSERT INTO repositories (owner_kind,owner_user_id,owner_org_id,name,description,is_private,disk_path,is_fork,forked_from_id,default_branch,storage_node_id,created_by_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT DO NOTHING RETURNING *",
    )
    .bind(kind)
    .bind(user)
    .bind(org)
    .bind(input.name)
    .bind(input.description)
    .bind(private)
    .bind(input.disk_path)
    .bind(source.is_some())
    .bind(input.source_id)
    .bind(source.as_ref().map_or("main", |repo| repo.default_branch.as_str()))
    .bind(input.storage_node_id)
    .bind(creator)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(repository)
}

pub async fn repository_visibility_locked(pool: &PgPool, repo_id: i64) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT visibility_locked FROM repositories WHERE id=$1")
            .bind(repo_id)
            .fetch_one(pool)
            .await?,
    )
}
