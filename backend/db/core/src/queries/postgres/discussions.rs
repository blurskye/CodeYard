use crate::models::AccessLevel;
use crate::queries::postgres::repositories::access::RepositoryRowLock as RepositoryLock;
use anyhow::Result;
use sqlx::Postgres;
use sqlx::Transaction;
pub struct DiscussionAuthority {
    pub access: AccessLevel,
    pub personal_override: bool,
    pub personal_owner: bool,
    pub organization: bool,
}

/// Lock the complete authority that supports a database effect. Organization
/// mutations lock their organization first; direct grants lock their own row.
pub async fn lock_discussion_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    lock: RepositoryLock,
    include_archived: bool,
) -> Result<Option<DiscussionAuthority>> {
    let Some(authority) =
        crate::queries::postgres::repositories::access::lock_user_repo_access_in_transaction(
            tx,
            actor_id,
            repo_id,
            lock,
            include_archived,
        )
        .await?
    else {
        return Ok(None);
    };
    if include_archived
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=$1)",
        )
        .bind(repo_id)
        .fetch_one(&mut **tx)
        .await?
    {
        return Ok(None);
    }
    // Public/read visibility survives suspension; contribution authority does not.
    if authority.organization && !authority.organization_usable {
        return Ok(None);
    }
    if !authority.is_admin
        && crate::queries::postgres::users::blocks::repository_interaction_blocked(
            tx, actor_id, repo_id,
        )
        .await?
    {
        return Ok(None);
    }
    Ok(Some(DiscussionAuthority {
        access: authority.access,
        personal_override: !authority.organization && authority.is_admin,
        personal_owner: !authority.organization && authority.owner_user_id == Some(actor_id),
        organization: authority.organization,
    }))
}

pub async fn lock_triage_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared, false)
            .await?
            .is_some_and(|a| a.access >= AccessLevel::Triage || a.personal_override),
    )
}

pub async fn lock_write_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared, false)
            .await?
            .is_some_and(|a| a.access.can_write() || a.personal_override),
    )
}

/// Repository policy mutation; reuse the locked current authority snapshot.
pub async fn lock_admin_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared, false)
            .await?
            .is_some_and(|a| a.access.can_admin() || a.personal_owner || a.personal_override),
    )
}

/// The same check as [`lock_admin_authority`] for a change to the repository row
/// itself: settings, rename and delete. The row is locked exclusively from the
/// start. Two such requests then queue up behind each other; with a shared lock
/// first and an upgrade later, both would hold the shared lock and wait for the
/// other to let go. (SQLite has one writer, so it needs no row lock.)
pub async fn lock_repository_mutation_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Exclusive, false)
            .await?
            .is_some_and(|a| a.access.can_admin() || a.personal_owner || a.personal_override),
    )
}

/// Security managers administer security state without repository-wide administration.
/// History reads include archived repositories; writers still require a mutable repository.
pub async fn lock_security_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    write: bool,
) -> Result<bool> {
    let Some(authority) =
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Exclusive, !write).await?
    else {
        return Ok(false);
    };
    if authority.access.can_admin() || authority.personal_owner || authority.personal_override {
        return Ok(true);
    }
    if !authority.organization {
        return Ok(false);
    }
    let org: Option<i64> = sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=$1")
        .bind(repo_id)
        .fetch_one(&mut **tx)
        .await?;
    match org {
        Some(org) => {
            crate::queries::postgres::organizations::roles::manages_security(tx, actor_id, org)
                .await
        }
        None => Ok(false),
    }
}

/// Administration that remains available on archived repositories, including deletion.
pub async fn lock_archive_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    lock_policy_authority_with(tx, actor_id, repo_id, RepositoryLock::Exclusive, false).await
}

/// Settings reads survive archival. Secret metadata still requires administration;
/// non-secret values require repository visibility, not contribution authority.
pub async fn lock_settings_read_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    admin: bool,
) -> Result<bool> {
    if admin {
        lock_policy_authority(tx, actor_id, repo_id, false).await
    } else {
        crate::queries::postgres::repositories::access::lock_readable_repository_in_transaction(
            tx,
            actor_id,
            repo_id,
            crate::queries::postgres::repositories::access::RepositoryRowLock::Shared,
        )
        .await
    }
}

/// Settings and bypass administration remain available after archival.
pub async fn lock_policy_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    maintain: bool,
) -> Result<bool> {
    lock_policy_authority_with(tx, actor_id, repo_id, RepositoryLock::Shared, maintain).await
}

pub async fn lock_policy_authority_with(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    lock: RepositoryLock,
    maintain: bool,
) -> Result<bool> {
    let authority =
        crate::queries::postgres::repositories::access::lock_user_repo_access_in_transaction(
            tx, actor_id, repo_id, lock, true,
        )
        .await?;
    let locked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=$1)",
    )
    .bind(repo_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(!locked
        && authority.is_some_and(|a| {
            (!a.organization || a.organization_usable)
                && ((if maintain {
                    a.access.can_maintain()
                } else {
                    a.access.can_admin()
                }) || (!a.organization && (a.is_admin || a.owner_user_id == Some(actor_id))))
        }))
}
