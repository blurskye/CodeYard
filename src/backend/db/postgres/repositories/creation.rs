use crate::backend::db::models::Repository;
use crate::backend::db::{
    models::OrgRole,
    organizations::{
        properties::{invalid as invalid_property, ValueInput},
        OrganizationOutcome,
    },
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
                OrganizationOutcome::Applied(OrgRole::Member) => {
                    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_settings WHERE org_id=$1 AND (members_can_create_public_repositories OR members_can_create_private_repositories))").bind(org).fetch_one(connection).await?;
                    anyhow::ensure!(allowed,RepositoryStorageRefusal::Denied);
                    Ok(false)
                }
                _ => Err(RepositoryStorageRefusal::Denied.into()),
            }
        }
        _ => Err(RepositoryStorageRefusal::Denied.into()),
    }
}

pub async fn check_creation_authority(pool: &PgPool, owner: RepositoryCreationOwner) -> Result<()> {
    let mut tx = pool.begin().await?;
    super::super::users::transitions::lock_membership_shared(&mut tx).await?;
    creation_authority(&mut tx, owner).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn validate_creation_visibility(
    c: &mut sqlx::PgConnection,
    owner: RepositoryCreationOwner,
    private: bool,
) -> Result<()> {
    if let RepositoryTarget::Organization(org) = owner.target {
        let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m LEFT JOIN organization_settings s ON s.org_id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2 AND (m.role IN ('owner','admin') OR (m.role='member' AND CASE WHEN $3 THEN s.members_can_create_private_repositories ELSE s.members_can_create_public_repositories END)))").bind(org).bind(owner.actor_id).bind(private).fetch_one(c).await?;
        anyhow::ensure!(allowed, RepositoryStorageRefusal::Denied);
    }
    Ok(())
}
pub(crate) async fn grant_creator(
    c: &mut sqlx::PgConnection,
    owner: RepositoryCreationOwner,
    repo: i64,
) -> Result<()> {
    if let RepositoryTarget::Organization(org) = owner.target {
        sqlx::query("INSERT INTO repo_access(user_id,repo_id,access_level) SELECT $1,$2,5 FROM org_memberships WHERE org_id=$3 AND user_id=$1 AND role='member' ON CONFLICT(user_id,repo_id) DO UPDATE SET access_level=5").bind(owner.actor_id).bind(repo).bind(org).execute(c).await?;
    }
    Ok(())
}
async fn fork_source(
    connection: &mut sqlx::PgConnection,
    actor: i64,
    admin: bool,
    id: i64,
) -> Result<Repository> {
    // Pin source-organization policy before the repository, matching mutation lock order.
    let original_org: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT owner_org_id FROM repositories WHERE id=$1 AND owner_kind='org'",
    )
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?
    .flatten();
    if let Some(org) = original_org {
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR SHARE")
            .bind(org)
            .fetch_all(&mut *connection)
            .await?;
    }
    // Lock the source row for update now: inserting the fork updates its fork
    // count, and two forks that each held a shared lock would deadlock on that.
    let source =
        sqlx::query_as::<_, Repository>("SELECT * FROM repositories WHERE id=$1 FOR NO KEY UPDATE")
            .bind(id)
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(RepositoryStorageRefusal::Denied)?;
    anyhow::ensure!(
        !super::super::users::blocks::repository_interaction_blocked(connection, actor, id).await?,
        RepositoryStorageRefusal::Denied
    );
    let access = super::access::user_repo_access_in_transaction(connection, actor, id).await?;
    // Site admins may fork any personal repository, not organization ones.
    let personal_admin = admin
        && source.owner_kind == "user"
        && source.owner_user_id.is_some()
        && source.owner_org_id.is_none();
    // Gist forks stage an unlisted source; initialize_fork rechecks live Git read access.
    if !(access.can_read() || personal_admin || (source.is_gist && !source.is_private)) {
        return Err(RepositoryStorageRefusal::Denied.into());
    }
    anyhow::ensure!(
        source.owner_org_id == original_org,
        RepositoryStorageRefusal::Denied
    );
    if source.is_private {
        if let Some(org) = source.owner_org_id {
            let allowed:bool=sqlx::query_scalar("SELECT COALESCE((SELECT members_can_fork_private_repositories FROM organization_settings WHERE org_id=$1),TRUE) OR EXISTS(SELECT 1 FROM org_memberships WHERE org_id=$1 AND user_id=$2 AND role IN ('owner','admin'))").bind(org).bind(actor).fetch_one(connection).await?;
            anyhow::ensure!(allowed, RepositoryStorageRefusal::Denied);
        }
    }
    Ok(source)
}

/// Inserts the repository row after checking the actor may create it here.
/// `None` means the name (or this user's fork of the source) already exists.
pub async fn insert_repository(
    pool: &PgPool,
    input: NewRepository<'_>,
) -> Result<Option<Repository>> {
    insert_repository_with_properties(pool, input, &[]).await
}
pub async fn insert_repository_with_properties(
    pool: &PgPool,
    input: NewRepository<'_>,
    properties: &[ValueInput],
) -> Result<Option<Repository>> {
    insert_kind(pool, input, false, properties).await
}
pub async fn insert_gist_repository(
    pool: &PgPool,
    input: NewRepository<'_>,
) -> Result<Option<Repository>> {
    anyhow::ensure!(
        matches!(input.owner.target, RepositoryTarget::Personal(_)) && !input.is_private,
        RepositoryStorageRefusal::Denied
    );
    insert_kind(pool, input, true, &[]).await
}
async fn insert_kind(
    pool: &PgPool,
    input: NewRepository<'_>,
    gist: bool,
    properties: &[ValueInput],
) -> Result<Option<Repository>> {
    let mut tx = pool.begin().await?;
    super::super::users::transitions::lock_membership_shared(&mut tx).await?;
    let creator = input.owner.actor_id;
    // Organizations lock in ascending id order. Otherwise a fork from A into B and one from B
    // into A each hold one organization and wait for the other.
    if let (Some(source), RepositoryTarget::Organization(destination)) =
        (input.source_id, input.owner.target)
    {
        let source_org: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT owner_org_id FROM repositories WHERE id=$1 AND owner_kind='org'",
        )
        .bind(source)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        if let Some(org) = source_org.filter(|org| *org < destination) {
            sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR SHARE")
                .bind(org)
                .fetch_all(&mut *tx)
                .await?;
        }
    }
    let admin = creation_authority(&mut tx, input.owner).await?;
    let (kind, user, org) = match input.owner.target {
        RepositoryTarget::Personal(id) => ("user", Some(id), None),
        RepositoryTarget::Organization(id) => ("org", None, Some(id)),
    };
    let property_values = if let Some(org) = org {
        super::super::organizations::properties::creation_values_on(
            &mut tx, creator, org, properties,
        )
        .await?
    } else {
        anyhow::ensure!(
            properties.is_empty(),
            invalid_property(
                "custom_properties",
                "Custom properties belong to organization repositories"
            )
        );
        Vec::new()
    };
    if !gist
        && !super::super::gists::ordinary_name_available(&mut tx, user, org, input.name).await?
    {
        return Ok(None);
    }
    // Lock the source first; a competing fork of it then waits here and sees this one.
    let source = match input.source_id {
        Some(id) => Some(fork_source(&mut tx, input.owner.actor_id, admin, id).await?),
        None => None,
    };
    anyhow::ensure!(
        source.as_ref().is_none_or(|source| source.is_gist == gist),
        RepositoryStorageRefusal::Denied
    );
    if source
        .as_ref()
        .is_some_and(|source| source.storage_node_id != input.storage_node_id)
    {
        return Err(RepositoryStorageRefusal::StalePlacement.into());
    }
    if let Some(source_id) = input.source_id {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE owner_kind=$1 AND (owner_user_id=$2 OR owner_org_id=$3) AND forked_from_id=$4)")
            .bind(kind).bind(user).bind(org)
            .bind(source_id)
            .fetch_one(&mut *tx)
            .await?;
        if exists {
            return Ok(None);
        }
    }
    let private = input.is_private || source.as_ref().is_some_and(|repo| repo.is_private);
    validate_creation_visibility(&mut tx, input.owner, private).await?;
    let repository = sqlx::query_as::<_, Repository>(
        "INSERT INTO repositories (owner_kind,owner_user_id,owner_org_id,name,description,is_private,disk_path,is_fork,forked_from_id,default_branch,storage_node_id,created_by_id,is_gist)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT DO NOTHING RETURNING *",
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
    .bind(gist)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(repository) = &repository {
        super::super::code_security::apply_default(&mut tx, repository).await?;
        if source.is_some() {
            sqlx::query("INSERT INTO repo_actions_settings(repo_id,enabled) VALUES($1,false)")
                .bind(repository.id).execute(&mut *tx).await?;
        }
    }
    if let (Some(org), Some(repository)) = (org, repository.as_ref()) {
        super::super::organizations::properties::apply_on(
            &mut tx,
            org,
            repository.id,
            &property_values,
        )
        .await?;
    }
    if let Some(repository) = repository.as_ref() {
        grant_creator(&mut tx, input.owner, repository.id).await?;
    }
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
