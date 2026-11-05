use crate::repositories::creation::CreationAdmission;
use crate::repositories::creation::NewRepository;
use crate::repositories::creation::RepositoryCreationOwner;
use crate::repositories::creation::RepositoryStorageRefusal;
use crate::repositories::creation::RepositoryTarget;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Repository;
use codeyard_db_core::pool::SqlitePool;
use codeyard_db_core::queries::organizations::properties::invalid as invalid_property;
use codeyard_db_core::queries::organizations::properties::ValueInput;
use codeyard_db_core::queries::organizations::OrganizationOutcome;

/// Whether the actor may create a repository for this owner; `Ok(true)` for site admins.
pub(crate) async fn creation_authority(
    connection: &mut sqlx::SqliteConnection,
    owner: RepositoryCreationOwner,
) -> Result<bool> {
    match owner.target {
        RepositoryTarget::Personal(user) if user == owner.actor_id => sqlx::query_scalar(
            "SELECT is_admin FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password",
        )
        .bind(owner.actor_id)
        .fetch_optional(connection)
        .await?
        .ok_or_else(|| RepositoryStorageRefusal::Denied.into()),
        RepositoryTarget::Organization(org) => {
            match codeyard_db_core::queries::sqlite::organizations::authority::authority(
                connection,
                owner.actor_id,
                org,
            )
            .await?
            {
                OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => {}
                OrganizationOutcome::Applied(OrgRole::Member) => {
                    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_settings WHERE org_id=?1 AND (members_can_create_public_repositories OR members_can_create_private_repositories))").bind(org).fetch_one(&mut *connection).await?;
                    anyhow::ensure!(allowed, RepositoryStorageRefusal::Denied);
                }
                _ => return Err(RepositoryStorageRefusal::Denied.into()),
            }
            // The destination does not change whether the actor is a site admin.
            Ok(sqlx::query_scalar::<_, bool>(
                "SELECT is_admin FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password",
            )
            .bind(owner.actor_id)
            .fetch_optional(&mut *connection)
            .await?
            .unwrap_or(false))
        }
        _ => Err(RepositoryStorageRefusal::Denied.into()),
    }
}

pub async fn check_creation_authority(
    pool: &SqlitePool,
    owner: RepositoryCreationOwner,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    creation_authority(&mut tx, owner).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn validate_creation_visibility(
    c: &mut sqlx::SqliteConnection,
    owner: RepositoryCreationOwner,
    private: bool,
) -> Result<()> {
    if let RepositoryTarget::Organization(org) = owner.target {
        let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m LEFT JOIN organization_settings s ON s.org_id=m.org_id WHERE m.org_id=?1 AND m.user_id=?2 AND (m.role IN ('owner','admin') OR (m.role='member' AND CASE WHEN ?3 THEN s.members_can_create_private_repositories ELSE s.members_can_create_public_repositories END)))").bind(org).bind(owner.actor_id).bind(private).fetch_one(c).await?;
        anyhow::ensure!(allowed, RepositoryStorageRefusal::Denied);
    }
    Ok(())
}
pub(crate) async fn grant_creator(
    c: &mut sqlx::SqliteConnection,
    owner: RepositoryCreationOwner,
    repo: i64,
) -> Result<()> {
    if let RepositoryTarget::Organization(org) = owner.target {
        sqlx::query("INSERT INTO repo_access(user_id,repo_id,access_level) SELECT ?1,?2,5 FROM org_memberships WHERE org_id=?3 AND user_id=?1 AND role='member' ON CONFLICT(user_id,repo_id) DO UPDATE SET access_level=5").bind(owner.actor_id).bind(repo).bind(org).execute(c).await?;
    }
    Ok(())
}
async fn fork_source(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    admin: bool,
    id: i64,
) -> Result<Repository> {
    let source = sqlx::query_as::<_, Repository>("SELECT * FROM repositories WHERE id=?1")
        .bind(id)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(RepositoryStorageRefusal::Denied)?;
    anyhow::ensure!(
        !codeyard_db_core::queries::sqlite::users::blocks::repository_interaction_blocked(
            connection, actor, id
        )
        .await?,
        RepositoryStorageRefusal::Denied
    );
    let access =
        codeyard_db_core::queries::sqlite::repositories::access::user_repo_access_in_transaction(
            connection, actor, id,
        )
        .await?;
    // Site admins may fork any personal repository, not organization ones.
    let personal_admin = admin
        && source.owner_kind == "user"
        && source.owner_user_id.is_some()
        && source.owner_org_id.is_none();
    // Gist forks stage an unlisted source; initialize_fork rechecks live Git read access.
    if !(access.can_read() || personal_admin || (source.is_gist && !source.is_private)) {
        return Err(RepositoryStorageRefusal::Denied.into());
    }
    if source.is_private {
        if let Some(org) = source.owner_org_id {
            let allowed:bool=sqlx::query_scalar("SELECT COALESCE((SELECT members_can_fork_private_repositories FROM organization_settings WHERE org_id=?1),1) OR EXISTS(SELECT 1 FROM org_memberships WHERE org_id=?1 AND user_id=?2 AND role IN ('owner','admin'))").bind(org).bind(actor).fetch_one(connection).await?;
            anyhow::ensure!(allowed, RepositoryStorageRefusal::Denied);
        }
    }
    Ok(source)
}

/// Inserts the repository row after checking the actor may create it here.
/// `None` means the name (or this user's fork of the source) already exists.
pub async fn insert_repository(
    pool: &SqlitePool,
    input: NewRepository<'_>,
) -> Result<Option<Repository>> {
    insert_repository_with_properties(pool, input, &[]).await
}
pub async fn insert_repository_with_properties(
    pool: &SqlitePool,
    input: NewRepository<'_>,
    properties: &[ValueInput],
) -> Result<Option<Repository>> {
    insert_kind(pool, input, false, properties, CreationAdmission::default()).await
}
pub async fn insert_repository_with_admission(
    pool: &SqlitePool,
    input: NewRepository<'_>,
    properties: &[ValueInput],
    admission: CreationAdmission<'_>,
) -> Result<Option<Repository>> {
    insert_kind(pool, input, false, properties, admission).await
}
pub async fn insert_gist_repository(
    pool: &SqlitePool,
    input: NewRepository<'_>,
) -> Result<Option<Repository>> {
    anyhow::ensure!(
        matches!(input.owner.target, RepositoryTarget::Personal(_)) && !input.is_private,
        RepositoryStorageRefusal::Denied
    );
    insert_kind(pool, input, true, &[], CreationAdmission::default()).await
}
async fn insert_kind(
    pool: &SqlitePool,
    input: NewRepository<'_>,
    gist: bool,
    properties: &[ValueInput],
    admission: CreationAdmission<'_>,
) -> Result<Option<Repository>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let creator = input.owner.actor_id;
    if let Some(authority) = admission.authority {
        anyhow::ensure!(
            authority.user_id == creator,
            RepositoryStorageRefusal::Denied
        );
    }
    let admin = creation_authority(&mut tx, input.owner).await?;
    let (kind, user, org) = match input.owner.target {
        RepositoryTarget::Personal(id) => ("user", Some(id), None),
        RepositoryTarget::Organization(id) => ("org", None, Some(id)),
    };
    let property_values = if let Some(org) = org {
        codeyard_db_orgs::sqlite::organizations::properties::creation_values_on(
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
        && !crate::sqlite::gists::ordinary_name_available(&mut tx, user, org, input.name).await?
    {
        return Ok(None);
    }
    // Same order as the PostgreSQL version: check the source, then look for an existing fork.
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
    if let Some(ghsa) = admission.advisory {
        anyhow::ensure!(
            !gist && input.is_private && input.owner.target == RepositoryTarget::Personal(creator),
            RepositoryStorageRefusal::Denied
        );
        let source = input.source_id.ok_or(RepositoryStorageRefusal::Denied)?;
        let advisory = codeyard_db_core::queries::sqlite::advisories::administration(
            &mut tx, creator, source, ghsa,
        )
        .await?;
        anyhow::ensure!(
            advisory.state == "draft" && advisory.private_fork_id.is_none(),
            codeyard_model::advisories::Refusal::Conflict
        );
    }
    if let Some(source_id) = input.source_id.filter(|_| admission.advisory.is_none()) {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE owner_kind=?1 AND (owner_user_id=?2 OR owner_org_id=?3) AND forked_from_id=?4)")
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
    // Retain the original credential after resource locks, checking expiry after admission waits.
    if let Some(credential) = admission.authority.and_then(|actor| actor.credential()) {
        codeyard_db_identity::sqlite::users::transitions::require_account(
            &mut tx,
            creator,
            &credential,
        )
        .await?;
    }
    if let Some(node) = input.storage_node_id {
        let eligible: Option<i64> = sqlx::query_scalar("SELECT id FROM nodes WHERE id=$1 AND status='active' AND state='active' AND EXISTS(SELECT 1 FROM json_each(roles) WHERE value='storage')")
            .bind(node).fetch_optional(&mut *tx).await?;
        anyhow::ensure!(eligible.is_some(), RepositoryStorageRefusal::StalePlacement);
    }
    let repository = sqlx::query_as::<_, Repository>(
        "INSERT INTO repositories (owner_kind,owner_user_id,owner_org_id,name,description,is_private,disk_path,is_fork,forked_from_id,default_branch,storage_node_id,created_by_id,is_gist,visibility_locked)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) ON CONFLICT DO NOTHING RETURNING *",
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
    .bind(admission.advisory.is_some())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(repository) = &repository {
        codeyard_db_core::queries::sqlite::code_security::apply_default(&mut tx, repository)
            .await?;
        if source.is_some() {
            sqlx::query("INSERT INTO repo_actions_settings(repo_id,enabled) VALUES(?1,false)")
                .bind(repository.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    if let (Some(org), Some(repository)) = (org, repository.as_ref()) {
        codeyard_db_orgs::sqlite::organizations::properties::apply_on(
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

pub async fn repository_visibility_locked(pool: &SqlitePool, repo_id: i64) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT visibility_locked FROM repositories WHERE id=?1")
            .bind(repo_id)
            .fetch_one(pool)
            .await?,
    )
}
