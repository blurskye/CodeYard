use crate::repositories::creation::CreationAdmission;
use crate::repositories::creation::NewRepository;
use crate::repositories::creation::RepositoryCreationOwner;
use crate::repositories::creation::RepositoryStorageRefusal;
use crate::repositories::creation::RepositoryTarget;
use anyhow::Result;
use codeyard_database::models::OrgRole;
use codeyard_database::models::Repository;
use codeyard_database::pool::PgPool;
use codeyard_database::queries::organizations::properties::invalid as invalid_property;
use codeyard_database::queries::organizations::properties::ValueInput;
use codeyard_database::queries::organizations::OrganizationOutcome;

/// Whether the actor may create a repository for this owner; `Ok(true)` for site admins.
pub(crate) async fn creation_authority(
    connection: &mut sqlx::PgConnection,
    owner: RepositoryCreationOwner,
) -> Result<bool> {
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        connection,
        &[owner.actor_id],
    )
    .await?;
    match owner.target {
        RepositoryTarget::Personal(user) if user == owner.actor_id => {
            sqlx::query_scalar("SELECT is_admin FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR SHARE")
                .bind(owner.actor_id).fetch_optional(connection).await?
                .ok_or_else(|| RepositoryStorageRefusal::Denied.into())
        }
        RepositoryTarget::Organization(org) => {
            match codeyard_database::queries::postgres::organizations::authority::authority(connection, owner.actor_id, org).await? {
                OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => {}
                OrganizationOutcome::Applied(OrgRole::Member) => {
                    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_settings WHERE org_id=$1 AND (members_can_create_public_repositories OR members_can_create_private_repositories))").bind(org).fetch_one(&mut *connection).await?;
                    anyhow::ensure!(allowed,RepositoryStorageRefusal::Denied);
                }
                _ => return Err(RepositoryStorageRefusal::Denied.into()),
            }
            // The destination does not change whether the actor is a site admin.
            Ok(sqlx::query_scalar::<_, bool>(
                "SELECT is_admin FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR SHARE",
            )
            .bind(owner.actor_id)
            .fetch_optional(&mut *connection)
            .await?
            .unwrap_or(false))
        }
        _ => Err(RepositoryStorageRefusal::Denied.into()),
    }
}

pub async fn check_creation_authority(pool: &PgPool, owner: RepositoryCreationOwner) -> Result<()> {
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[owner.actor_id],
    )
    .await?;
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
        !codeyard_database::queries::postgres::users::blocks::repository_interaction_blocked(
            connection, actor, id
        )
        .await?,
        RepositoryStorageRefusal::Denied
    );
    let access =
        codeyard_database::queries::postgres::repositories::access::user_repo_access_in_transaction(
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
    insert_kind(pool, input, false, properties, CreationAdmission::default()).await
}
pub async fn insert_repository_with_admission(
    pool: &PgPool,
    input: NewRepository<'_>,
    properties: &[ValueInput],
    admission: CreationAdmission<'_>,
) -> Result<Option<Repository>> {
    insert_kind(pool, input, false, properties, admission).await
}
pub async fn insert_gist_repository(
    pool: &PgPool,
    input: NewRepository<'_>,
) -> Result<Option<Repository>> {
    anyhow::ensure!(
        matches!(input.owner.target, RepositoryTarget::Personal(_)) && !input.is_private,
        RepositoryStorageRefusal::Denied
    );
    insert_kind(pool, input, true, &[], CreationAdmission::default()).await
}
async fn insert_kind(
    pool: &PgPool,
    input: NewRepository<'_>,
    gist: bool,
    properties: &[ValueInput],
    admission: CreationAdmission<'_>,
) -> Result<Option<Repository>> {
    let mut tx = pool.begin().await?;
    crate::postgres::gists::lock_name(&mut tx, input.name).await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[input.owner.actor_id],
    )
    .await?;
    let creator = input.owner.actor_id;
    if let Some(authority) = admission.authority {
        anyhow::ensure!(
            authority.user_id == creator,
            RepositoryStorageRefusal::Denied
        );
    }
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
        codeyard_db_orgs::postgres::organizations::properties::creation_values_on(
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
        && !crate::postgres::gists::ordinary_name_available(&mut tx, user, org, input.name).await?
    {
        return Ok(None);
    }
    // Lock the source first; a competing fork of it then waits here and sees this one.
    anyhow::ensure!(
        admission.foreign_source.is_none() || input.source_id.is_some(),
        RepositoryStorageRefusal::Denied
    );
    let source = match (input.source_id, admission.foreign_source) {
        (Some(id), Some(foreign)) => {
            anyhow::ensure!(
                !gist
                    && admission.advisory.is_none()
                    && foreign.actor.user_id == creator
                    && admission.authority == Some(&foreign.actor)
                    && foreign.source.repository.id == id,
                RepositoryStorageRefusal::Denied
            );
            let stored = sqlx::query_as::<_, Repository>(
                "SELECT * FROM repositories WHERE id=$1 FOR NO KEY UPDATE",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(RepositoryStorageRefusal::Denied)?;
            anyhow::ensure!(
                stored.is_stub
                    && stored.home_cell == foreign.source.repository.home_cell
                    && stored.home_cell != i32::from(pool.cell_id().get()),
                RepositoryStorageRefusal::Denied
            );
            Some(
                foreign
                    .check(&codeyard_database::DbPool::Postgres(pool.clone()))
                    .await?
                    .repository,
            )
        }
        (Some(id), None) => Some(fork_source(&mut tx, input.owner.actor_id, admin, id).await?),
        (None, None) => None,
        (None, Some(_)) => return Err(RepositoryStorageRefusal::Denied.into()),
    };
    anyhow::ensure!(
        source.as_ref().is_none_or(|source| source.is_gist == gist),
        RepositoryStorageRefusal::Denied
    );
    if admission.foreign_source.is_none()
        && source
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
        let advisory = codeyard_database::queries::postgres::advisories::administration(
            &mut tx, creator, source, ghsa,
        )
        .await?;
        anyhow::ensure!(
            advisory.state == "draft" && advisory.private_fork_id.is_none(),
            codeyard_types::advisories::Refusal::Conflict
        );
    }
    if let Some(source_id) = input.source_id.filter(|_| admission.advisory.is_none()) {
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
    // Retain the original credential after resource locks, checking expiry after admission waits.
    if let Some(credential) = admission.authority.and_then(|actor| actor.credential()) {
        codeyard_db_identity::postgres::users::transitions::require_account(
            &mut tx,
            creator,
            &credential,
        )
        .await?;
    }
    if let Some(node) = input.storage_node_id {
        let eligible: Option<i64> = sqlx::query_scalar("SELECT id FROM nodes WHERE id=$1 AND status='active' AND state='active' AND 'storage'=ANY(roles) FOR SHARE")
            .bind(node).fetch_optional(&mut *tx).await?;
        anyhow::ensure!(eligible.is_some(), RepositoryStorageRefusal::StalePlacement);
    }
    let repository = sqlx::query_as::<_, Repository>(
        sqlx::AssertSqlSafe(format!("INSERT INTO repositories (home_cell,owner_kind,owner_user_id,owner_org_id,name,description,is_private,disk_path,is_fork,forked_from_id,default_branch,storage_node_id,created_by_id,is_gist,visibility_locked)
         VALUES ({cell_home},$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) ON CONFLICT DO NOTHING RETURNING *", cell_home = codeyard_database::cells::LOCAL_HOME_SQL)),
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
        codeyard_database::queries::postgres::code_security::apply_default(&mut tx, repository)
            .await?;
        if source.is_some() {
            sqlx::query("INSERT INTO repo_actions_settings(repo_id,enabled) VALUES($1,false)")
                .bind(repository.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    if let (Some(org), Some(repository)) = (org, repository.as_ref()) {
        codeyard_db_orgs::postgres::organizations::properties::apply_on(
            &mut tx,
            org,
            repository.id,
            &property_values,
        )
        .await?;
    }
    if let Some(repository) = repository.as_ref() {
        grant_creator(&mut tx, input.owner, repository.id).await?;
        codeyard_database::cells::directory::postgres::repository(pool, &mut tx, repository)
            .await?;
    }
    if let Some(foreign) = admission.foreign_source {
        let current = foreign
            .check(&codeyard_database::DbPool::Postgres(pool.clone()))
            .await?;
        anyhow::ensure!(
            (!current.repository.is_private || private)
                && source.as_ref().is_some_and(
                    |source| source.default_branch == current.repository.default_branch
                ),
            RepositoryStorageRefusal::StalePlacement
        );
    }
    if let Some(credential) = admission.authority.and_then(|actor| actor.credential()) {
        codeyard_db_identity::postgres::users::transitions::require_account(
            &mut tx,
            creator,
            &credential,
        )
        .await?;
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

/// Reads the native source with the same fork policy and retained credential used by creation.
pub async fn read_fork_source(
    pool: &PgPool,
    actor: &codeyard_database::queries::credentials::Actor,
    id: i64,
) -> Result<Option<Repository>> {
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[actor.user_id],
    )
    .await?;
    let admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR SHARE")
        .bind(actor.user_id).fetch_optional(&mut *tx).await?;
    let Some(admin) = admin else {
        return Ok(None);
    };
    let source = match fork_source(&mut tx, actor.user_id, admin, id).await {
        Ok(source) => source,
        Err(error) if error.is::<RepositoryStorageRefusal>() => return Ok(None),
        Err(error) => return Err(error),
    };
    if source.is_stub || source.is_gist || source.home_cell != i32::from(pool.cell_id().get()) {
        return Ok(None);
    }
    if source.is_private {
        let allowed: bool = sqlx::query_scalar("SELECT COALESCE((SELECT allow_forking FROM repository_features WHERE repo_id=$1),TRUE)")
            .bind(id).fetch_one(&mut *tx).await?;
        if !allowed {
            return Ok(None);
        }
    }
    if !codeyard_database::queries::postgres::credentials::repository(
        &mut tx, actor, id, "contents", false,
    )
    .await?
    {
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(source))
}

/// A native metadata/content read retains the original credential and repository authority.
pub async fn read_repository_source(
    pool: &PgPool,
    actor: &codeyard_database::queries::credentials::Actor,
    id: i64,
) -> Result<Option<Repository>> {
    let mut tx = pool.begin().await?;
    if !codeyard_database::queries::postgres::repositories::access::lock_readable_repository_in_transaction(
        &mut tx, actor.user_id, id, codeyard_database::queries::postgres::repositories::access::RepositoryRowLock::Shared,
    ).await? || !codeyard_database::queries::postgres::credentials::repository(
        &mut tx, actor, id, "contents", false,
    ).await? {
        return Ok(None);
    }
    let source: Option<Repository> = sqlx::query_as(
        "SELECT * FROM repositories WHERE id=$1 AND NOT is_stub AND NOT is_gist AND home_cell=$2",
    )
    .bind(id)
    .bind(i32::from(pool.cell_id().get()))
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(source)
}
