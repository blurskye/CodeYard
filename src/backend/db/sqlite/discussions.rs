// The caller's revision is never replaced by a freshly loaded revision.
use crate::backend::db::models::AccessLevel;
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

pub async fn last_discussion_numbers(
    pool: &SqlitePool,
    repo_ids: &[i64],
) -> Result<Vec<(i64, i64)>> {
    if repo_ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(sqlx::query_as(
        "SELECT repo_id, next_number - 1 FROM repository_discussion_counters WHERE repo_id IN (SELECT value FROM json_each(?1))",
    )
    .bind(serde_json::to_string(repo_ids)?)
    .fetch_all(pool)
    .await?)
}

/// The label names a list narrows to, as the JSON array that `json_each` reads. None when the
/// list has no label filter.
pub(crate) fn label_names_json(labels: &[String]) -> Result<Option<String>> {
    Ok(if labels.is_empty() {
        None
    } else {
        Some(serde_json::to_string(labels)?)
    })
}

pub(crate) const AUTHORITY_DENIED: crate::backend::db::discussions::DiscussionWriteRefusal =
    crate::backend::db::discussions::DiscussionWriteRefusal::Denied;

struct DiscussionAuthority {
    access: AccessLevel,
    personal_override: bool,
    personal_owner: bool,
    organization: bool,
}

/// Called only inside BEGIN IMMEDIATE. The writer reservation serializes
/// account, tenant and grant mutations through allocation and mutation commit.
async fn lock_discussion_authority(
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
    if !admin && super::users::blocks::repository_interaction_blocked(tx, actor_id, repo_id).await?
    {
        return Ok(None);
    }
    let access =
        super::repositories::access::user_repo_access_in_transaction(tx, actor_id, repo_id).await?;
    Ok(Some(DiscussionAuthority {
        access,
        personal_override: !organization && admin,
        personal_owner: !organization && owner == Some(actor_id),
        organization,
    }))
}

/// An effect by a configured repository reader, without issue interaction restrictions.
pub(crate) async fn lock_read_authority(
    tx: &mut Transaction<'_, Sqlite>, actor_id: i64, repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false).await?
        .is_some_and(|authority| authority.access.can_read() || authority.personal_override))
}

pub(crate) async fn lock_triage_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false)
        .await?
        .is_some_and(|a| a.access >= AccessLevel::Triage || a.personal_override))
}

pub(crate) async fn lock_write_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false)
        .await?
        .is_some_and(|a| a.access.can_write() || a.personal_override))
}

/// Moderation of other people's comments: repository maintainers and site administrators.
pub(crate) async fn lock_maintain_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false)
        .await?
        .is_some_and(|a| a.access.can_maintain() || a.personal_override))
}

/// Repository policy mutation; reuse the locked current authority snapshot.
pub(crate) async fn lock_admin_authority(
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
pub(crate) async fn lock_security_authority(
    tx: &mut Transaction<'_, Sqlite>, actor_id: i64, repo_id: i64, write: bool,
) -> Result<bool> {
    let Some(authority) = lock_discussion_authority(tx, actor_id, repo_id, !write).await? else { return Ok(false); };
    if authority.access.can_admin() || authority.personal_owner || authority.personal_override { return Ok(true); }
    if !authority.organization { return Ok(false); }
    let org: Option<i64> = sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=?1").bind(repo_id).fetch_one(&mut **tx).await?;
    match org {
        Some(org) => super::organizations::roles::manages_security(tx, actor_id, org).await,
        None => Ok(false),
    }
}

/// Administration of archival state must also work when the repository is already archived.
pub(crate) async fn lock_archive_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    lock_policy_authority(tx, actor_id, repo_id, false).await
}

/// Settings reads survive archival. Secret metadata still requires administration;
/// non-secret values require repository visibility, not contribution authority.
pub(crate) async fn lock_settings_read_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
    admin: bool,
) -> Result<bool> {
    if admin {
        lock_policy_authority(tx, actor_id, repo_id, false).await
    } else {
        super::repositories::access::lock_readable_repository_in_transaction(tx, actor_id, repo_id).await
    }
}

/// Settings and bypass administration remain available after archival.
pub(crate) async fn lock_policy_authority(
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

/// Whether the actor may still take part in the repository (read access is enough).
pub(crate) async fn lock_contribution_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    let authority = lock_discussion_authority(tx, actor_id, repo_id, false).await?;
    let Some(authority) = authority else {
        return Ok(false);
    };
    if !(authority.access.can_read() || authority.personal_override)
        || super::users::blocks::repository_interaction_blocked(tx, actor_id, repo_id).await?
    {
        return Ok(false);
    }
    super::interactions::allows_contribution(
        tx,
        actor_id,
        repo_id,
        authority.access.can_write() || authority.personal_override,
    )
    .await
}

pub(crate) async fn require_contribution_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<()> {
    anyhow::ensure!(
        lock_contribution_authority(tx, actor_id, repo_id).await?,
        AUTHORITY_DENIED
    );
    Ok(())
}

pub async fn set_lock(
    pool: &sqlx::SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    item: crate::backend::db::assignees::AssignedItem,
    locked: bool,
    reason: Option<&str>,
) -> Result<()> {
    use crate::backend::db::{assignees::AssignedItem, discussions::LockRefusal};
    anyhow::ensure!(
        crate::backend::db::discussions::valid_lock_reason(reason),
        LockRefusal::InvalidReason
    );
    let actor = actor.into();
    let actor_id = actor.user_id;
    let reason = if locked { reason } else { None };
    let (table, subject, id, permission) = match item {
        AssignedItem::Issue(id) => ("issues", "issue", id, "issues"),
        AssignedItem::PullRequest(id) => ("pull_requests", "pull_request", id, "pull_requests"),
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        lock_triage_authority(&mut tx, actor_id, repo_id).await?
            && super::credentials::repository(&mut tx, &actor, repo_id, permission, true).await?,
        LockRefusal::Denied
    );
    let (previous, previous_reason, updated): (
        bool,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as(&format!(
        "SELECT locked,lock_reason,updated_at FROM {table} WHERE id=?1 AND repo_id=?2"
    ))
    .bind(id)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(LockRefusal::Missing)?;
    if previous != locked || previous_reason.as_deref() != reason {
        let next = super::time::next_revision(updated)?;
        sqlx::query(&format!(
            "UPDATE {table} SET locked=?2,lock_reason=?3,updated_at=?4 WHERE id=?1"
        ))
        .bind(id)
        .bind(locked)
        .bind(reason)
        .bind(next)
        .execute(&mut *tx)
        .await?;
        super::timeline::append(&mut tx, repo_id, subject, id, actor_id, if locked {"locked"} else {"unlocked"}, reason.unwrap_or(""), "").await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn lock_issue_contribution(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    issue_id: i64,
) -> Result<()> {
    let issue: Option<(i64, bool)> =
        sqlx::query_as("SELECT repo_id, locked FROM issues WHERE id=?1")
            .bind(issue_id)
            .fetch_optional(&mut **tx)
            .await?;
    let (repo, locked) = issue.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    require_contribution_authority(tx, actor_id, repo).await?;
    anyhow::ensure!(
        !locked,
        crate::backend::db::discussions::DiscussionWriteRefusal::Locked
    );
    Ok(())
}

pub(crate) async fn lock_pull_request_contribution(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    pr_id: i64,
) -> Result<()> {
    let scope: Option<(i64, bool)> =
        sqlx::query_as("SELECT repo_id,locked FROM pull_requests WHERE id=?1")
            .bind(pr_id)
            .fetch_optional(&mut **tx)
            .await?;
    let (repo, locked) = scope.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    require_contribution_authority(tx, actor_id, repo).await?;
    anyhow::ensure!(
        !locked,
        crate::backend::db::discussions::DiscussionWriteRefusal::Locked
    );
    Ok(())
}

pub(crate) async fn lock_pull_request_review_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    pr_id: i64,
) -> Result<(i64, String, String)> {
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=?1")
        .bind(pr_id)
        .fetch_optional(&mut **tx)
        .await?;
    let repo = repo.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    anyhow::ensure!(
        lock_write_authority(tx, actor_id, repo).await?,
        AUTHORITY_DENIED
    );
    lock_pull_request_contribution(tx, actor_id, pr_id).await?;
    read_pull_request_review_state(tx, pr_id).await
}

/// The author, state and head of a pull request. Call it only after the caller's authority
/// check in the same transaction.
pub(crate) async fn read_pull_request_review_state(
    tx: &mut Transaction<'_, Sqlite>,
    pr_id: i64,
) -> Result<(i64, String, String)> {
    sqlx::query_as("SELECT author_id,state,head_sha FROM pull_requests WHERE id=?1")
        .bind(pr_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))
}

/// Resolving a conversation takes write access, or the pull request's author while they can
/// still read the repository. Both need the live actor, unarchived repository and usable owner
/// that every discussion write needs.
pub(crate) async fn lock_comment_resolution_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    pr_id: i64,
) -> Result<()> {
    let scope: Option<(i64, i64)> =
        sqlx::query_as("SELECT repo_id, author_id FROM pull_requests WHERE id=?1")
            .bind(pr_id)
            .fetch_optional(&mut **tx)
            .await?;
    let (repo, author) = scope.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    anyhow::ensure!(
        lock_discussion_authority(tx, actor_id, repo, false)
            .await?
            .is_some_and(|a| a.access.can_write()
                || a.personal_override
                || (author == actor_id && a.access.can_read())),
        AUTHORITY_DENIED
    );
    Ok(())
}

pub(crate) async fn lock_review_dismissal_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    review_id: i64,
) -> Result<i64> {
    let scope: Option<(i64, i64)> = sqlx::query_as("SELECT p.id,p.repo_id FROM pr_reviews r JOIN pull_requests p ON p.id=r.pull_request_id WHERE r.id=?1")
        .bind(review_id).fetch_optional(&mut **tx).await?;
    let (pr, repo) = scope.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    anyhow::ensure!(
        lock_discussion_authority(tx, actor_id, repo, false)
            .await?
            .is_some_and(|a| if a.organization {
                a.access.can_admin()
            } else {
                a.personal_owner || a.personal_override
            }),
        AUTHORITY_DENIED
    );
    let present: Option<i64> =
        sqlx::query_scalar("SELECT id FROM pull_requests WHERE id=?1 AND repo_id=?2")
            .bind(pr)
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?;
    anyhow::ensure!(present.is_some(), AUTHORITY_DENIED);
    Ok(pr)
}

pub(super) async fn allocate_number(tx: &mut Transaction<'_, Sqlite>, repo_id: i64) -> Result<i32> {
    // Counter allocation rolls back with the issue/PR insertion.
    let allocated: i64 = sqlx::query_scalar(
        "INSERT INTO repository_discussion_counters(repo_id,next_number) VALUES (?1,2) ON CONFLICT(repo_id) DO UPDATE SET next_number=next_number+1 RETURNING next_number-1",
    ).bind(repo_id).fetch_one(&mut **tx).await?;
    Ok(i32::try_from(allocated)?)
}

pub async fn list_repo_authors(pool: &SqlitePool, repo_id: i64, selected: Option<i64>) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(&crate::backend::db::discussions::author_choices_sql(false))
        .bind(repo_id).bind(selected).fetch_all(pool).await?)
}
