use crate::models::AccessLevel;
use anyhow::Result;
use sqlx::Sqlite;
use sqlx::Transaction;
pub struct DiscussionAuthority {
    pub access: AccessLevel,
    pub personal_override: bool,
    personal_owner: bool,
    organization: bool,
}

/// Called only inside BEGIN IMMEDIATE. The writer reservation serializes
/// account, tenant and grant mutations through allocation and mutation commit.
pub async fn lock_discussion_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
    include_archived: bool,
) -> Result<Option<DiscussionAuthority>> {
    let actor: Option<(bool, bool, bool)> =
        sqlx::query_as("SELECT is_admin, is_banned, must_change_password FROM users WHERE id = ?1")
            .bind(actor_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((admin, false, false)) = actor else {
        return Ok(None);
    };
    let repo: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT owner_kind, owner_user_id, owner_org_id FROM repositories WHERE id = ?1 AND NOT is_gist AND (?2 OR NOT is_archived) AND NOT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=?1)",
    ).bind(repo_id).bind(include_archived).fetch_optional(&mut **tx).await?;
    let Some((kind, owner, org)) = repo else {
        return Ok(None);
    };
    let organization = match (kind.as_str(), owner, org) {
        ("user", Some(owner), None) => {
            let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1)")
                .bind(owner)
                .fetch_one(&mut **tx)
                .await?;
            if !exists {
                return Ok(None);
            }
            false
        }
        ("org", None, Some(org)) => {
            // Public/read visibility survives suspension; contribution authority
            // does not. Only an active original owner supplies usable governance.
            let usable: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM organizations o JOIN org_memberships m ON m.org_id=o.id AND m.role='owner' JOIN users u ON u.id=m.user_id WHERE o.id=?1 AND NOT u.is_banned AND NOT u.must_change_password)",
            ).bind(org).fetch_one(&mut **tx).await?;
            if !usable {
                return Ok(None);
            }
            true
        }
        _ => return Ok(None),
    };
    if !admin
        && crate::queries::sqlite::users::blocks::repository_interaction_blocked(
            tx, actor_id, repo_id,
        )
        .await?
    {
        return Ok(None);
    }
    let access = crate::queries::sqlite::repositories::access::user_repo_access_in_transaction(
        tx, actor_id, repo_id,
    )
    .await?;
    Ok(Some(DiscussionAuthority {
        access,
        personal_override: !organization && admin,
        personal_owner: !organization && owner == Some(actor_id),
        organization,
    }))
}

pub async fn lock_triage_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false)
        .await?
        .is_some_and(|a| a.access >= AccessLevel::Triage || a.personal_override))
}

pub async fn lock_write_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false)
        .await?
        .is_some_and(|a| a.access.can_write() || a.personal_override))
}

/// Repository policy mutation; reuse the locked current authority snapshot.
pub async fn lock_admin_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false)
        .await?
        .is_some_and(|a| a.access.can_admin() || a.personal_owner || a.personal_override))
}

/// Security managers administer security state without repository-wide administration.
/// History reads include archived repositories; writers still require a mutable repository.
pub async fn lock_security_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
    write: bool,
) -> Result<bool> {
    let Some(authority) = lock_discussion_authority(tx, actor_id, repo_id, !write).await? else {
        return Ok(false);
    };
    if authority.access.can_admin() || authority.personal_owner || authority.personal_override {
        return Ok(true);
    }
    if !authority.organization {
        return Ok(false);
    }
    let org: Option<i64> = sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=?1")
        .bind(repo_id)
        .fetch_one(&mut **tx)
        .await?;
    match org {
        Some(org) => {
            crate::queries::sqlite::organizations::roles::manages_security(tx, actor_id, org).await
        }
        None => Ok(false),
    }
}

/// Administration of archival state must also work when the repository is already archived.
pub async fn lock_archive_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    lock_policy_authority(tx, actor_id, repo_id, false).await
}

/// Settings reads survive archival. Secret metadata still requires administration;
/// non-secret values require repository visibility, not contribution authority.
pub async fn lock_settings_read_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
    admin: bool,
) -> Result<bool> {
    if admin {
        lock_policy_authority(tx, actor_id, repo_id, false).await
    } else {
        crate::queries::sqlite::repositories::access::lock_readable_repository_in_transaction(
            tx, actor_id, repo_id,
        )
        .await
    }
}

/// Settings and bypass administration remain available after archival.
pub async fn lock_policy_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
    maintain: bool,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, true)
        .await?
        .is_some_and(|a| {
            (if maintain {
                a.access.can_maintain()
            } else {
                a.access.can_admin()
            }) || a.personal_owner
                || a.personal_override
        }))
}
