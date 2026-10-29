// The caller's revision is never replaced by a freshly loaded revision.
use super::repositories::access::RepositoryRowLock as RepositoryLock;
use crate::backend::db::models::AccessLevel;
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

/// The label names a list narrows to, as the text array that `= ANY` reads. None when the list
/// has no label filter.
pub(crate) fn label_names_array(labels: &[String]) -> Option<Vec<String>> {
    (!labels.is_empty()).then(|| labels.to_vec())
}

pub async fn last_discussion_numbers(pool: &PgPool, repo_ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    if repo_ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(sqlx::query_as(
        "SELECT repo_id, next_number - 1 FROM repository_discussion_counters WHERE repo_id = ANY($1)",
    )
    .bind(repo_ids)
    .fetch_all(pool)
    .await?)
}

pub(crate) const AUTHORITY_DENIED: crate::backend::db::discussions::DiscussionWriteRefusal =
    crate::backend::db::discussions::DiscussionWriteRefusal::Denied;

struct DiscussionAuthority {
    access: AccessLevel,
    personal_override: bool,
    personal_owner: bool,
    organization: bool,
}

/// Lock the complete authority that supports a database effect. Organization
/// mutations lock their organization first; direct grants lock their own row.
async fn lock_discussion_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    lock: RepositoryLock,
) -> Result<Option<DiscussionAuthority>> {
    let Some(authority) = super::repositories::access::lock_user_repo_access_in_transaction(
        tx, actor_id, repo_id, lock, false,
    )
    .await?
    else {
        return Ok(None);
    };
    // Public/read visibility survives suspension; contribution authority does not.
    if authority.organization && !authority.organization_usable {
        return Ok(None);
    }
    if !authority.is_admin
        && super::users::blocks::repository_interaction_blocked(tx, actor_id, repo_id).await?
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

/// An effect by a configured repository reader, without issue interaction restrictions.
pub(crate) async fn lock_read_authority(
    tx: &mut Transaction<'_, Postgres>, actor_id: i64, repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared).await?
        .is_some_and(|authority| authority.access.can_read() || authority.personal_override))
}

pub(crate) async fn lock_triage_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared)
            .await?
            .is_some_and(|a| a.access >= AccessLevel::Triage || a.personal_override),
    )
}

pub(crate) async fn lock_write_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared)
            .await?
            .is_some_and(|a| a.access.can_write() || a.personal_override),
    )
}

/// [`lock_write_authority`] with the repository row locked exclusively from the start, for a
/// write that must queue behind other writers of the repository's pull requests.
pub(crate) async fn lock_exclusive_write_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Exclusive)
            .await?
            .is_some_and(|a| a.access.can_write() || a.personal_override),
    )
}

/// Moderation of other people's comments: repository maintainers and site administrators.
pub(crate) async fn lock_maintain_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared)
            .await?
            .is_some_and(|a| a.access.can_maintain() || a.personal_override),
    )
}

/// Repository policy mutation; reuse the locked current authority snapshot.
pub(crate) async fn lock_admin_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Shared)
            .await?
            .is_some_and(|a| a.access.can_admin() || a.personal_owner || a.personal_override),
    )
}

/// The same check as [`lock_admin_authority`] for a change to the repository row
/// itself: settings, rename and delete. The row is locked exclusively from the
/// start. Two such requests then queue up behind each other; with a shared lock
/// first and an upgrade later, both would hold the shared lock and wait for the
/// other to let go. (SQLite has one writer, so it needs no row lock.)
pub(crate) async fn lock_repository_mutation_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        lock_discussion_authority(tx, actor_id, repo_id, RepositoryLock::Exclusive)
            .await?
            .is_some_and(|a| a.access.can_admin() || a.personal_owner || a.personal_override),
    )
}

/// Administration that remains available on archived repositories, including deletion.
pub(crate) async fn lock_archive_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    lock_policy_authority_with(tx, actor_id, repo_id, RepositoryLock::Exclusive, false).await
}

/// Reading repository settings. Administrators may read them on archived repositories too;
/// with `admin` false, anyone who may take part in the repository may also read them.
pub(crate) async fn lock_settings_read_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    admin: bool,
) -> Result<bool> {
    if !admin && lock_contribution_authority(tx, actor_id, repo_id).await? {
        return Ok(true);
    }
    lock_policy_authority(tx, actor_id, repo_id, false).await
}

/// Settings and bypass administration remain available after archival.
pub(crate) async fn lock_policy_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    maintain: bool,
) -> Result<bool> {
    lock_policy_authority_with(tx, actor_id, repo_id, RepositoryLock::Shared, maintain).await
}
async fn lock_policy_authority_with(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    lock: RepositoryLock,
    maintain: bool,
) -> Result<bool> {
    let authority = super::repositories::access::lock_user_repo_access_in_transaction(
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

async fn lock_contribution_authority_with(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
    repository_lock: RepositoryLock,
) -> Result<bool> {
    let authority = lock_discussion_authority(tx, actor_id, repo_id, repository_lock).await?;
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

/// `lock_contribution_authority` with an exclusive repository lock, for admission that
/// takes its repository lock before any pull request or workflow row, like ordinary CI admission.
pub(crate) async fn lock_contribution_authority_exclusive(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    lock_contribution_authority_with(tx, actor_id, repo_id, RepositoryLock::Exclusive).await
}

/// Whether the actor may still take part in the repository (read access is enough).
pub(crate) async fn lock_contribution_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    lock_contribution_authority_with(tx, actor_id, repo_id, RepositoryLock::Shared).await
}

pub(crate) async fn require_contribution_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<()> {
    anyhow::ensure!(
        lock_contribution_authority(tx, actor_id, repo_id).await?,
        AUTHORITY_DENIED
    );
    Ok(())
}

/// Serialize repository-scoped creation without taking the repository parent
/// before its organization and account authority parents.
pub(crate) async fn require_exclusive_contribution_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    repo_id: i64,
) -> Result<()> {
    anyhow::ensure!(
        lock_contribution_authority_with(tx, actor_id, repo_id, RepositoryLock::Exclusive).await?,
        AUTHORITY_DENIED
    );
    Ok(())
}

pub async fn set_lock(
    pool: &sqlx::PgPool,
    actor_id: i64,
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
    let reason = if locked { reason } else { None };
    let (table, subject, id) = match item {
        AssignedItem::Issue(id) => ("issues", "issue", id),
        AssignedItem::PullRequest(id) => ("pull_requests", "pull_request", id),
    };
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        lock_write_authority(&mut tx, actor_id, repo_id).await?,
        LockRefusal::Denied
    );
    let (previous, previous_reason): (bool, Option<String>) = sqlx::query_as(&format!(
        "SELECT locked,lock_reason FROM {table} WHERE id=$1 AND repo_id=$2 FOR UPDATE"
    ))
    .bind(id)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(LockRefusal::Missing)?;
    if previous != locked || previous_reason.as_deref() != reason {
        sqlx::query(&format!("UPDATE {table} SET locked=$2,lock_reason=$3,updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1")).bind(id).bind(locked).bind(reason).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO timeline_events(repo_id,subject_kind,subject_id,actor_id,kind,detail) VALUES ($1,$2,$3,$4,$5,$6)").bind(repo_id).bind(subject).bind(id).bind(actor_id).bind(if locked {"locked"} else {"unlocked"}).bind(reason.unwrap_or("")).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn lock_issue_contribution(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    issue_id: i64,
) -> Result<()> {
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id=$1")
        .bind(issue_id)
        .fetch_optional(&mut **tx)
        .await?;
    let repo = repo.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    require_contribution_authority(tx, actor_id, repo).await?;
    let locked: Option<bool> =
        sqlx::query_scalar("SELECT locked FROM issues WHERE id=$1 AND repo_id=$2 FOR UPDATE")
            .bind(issue_id)
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?;
    anyhow::ensure!(locked.is_some(), AUTHORITY_DENIED);
    anyhow::ensure!(
        locked == Some(false),
        crate::backend::db::discussions::DiscussionWriteRefusal::Locked
    );
    Ok(())
}

pub(crate) async fn lock_pull_request_contribution(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    pr_id: i64,
) -> Result<()> {
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=$1")
        .bind(pr_id)
        .fetch_optional(&mut **tx)
        .await?;
    let repo = repo.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    require_contribution_authority(tx, actor_id, repo).await?;
    let locked: Option<bool> = sqlx::query_scalar(
        "SELECT locked FROM pull_requests WHERE id=$1 AND repo_id=$2 FOR UPDATE",
    )
    .bind(pr_id)
    .bind(repo)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(locked.is_some(), AUTHORITY_DENIED);
    anyhow::ensure!(
        locked == Some(false),
        crate::backend::db::discussions::DiscussionWriteRefusal::Locked
    );
    Ok(())
}

pub(crate) async fn lock_pull_request_review_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    pr_id: i64,
) -> Result<(i64, String, String)> {
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=$1")
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

/// The author, state and head of a pull request, locked. Call it only after the caller's
/// authority check in the same transaction.
pub(crate) async fn read_pull_request_review_state(
    tx: &mut Transaction<'_, Postgres>,
    pr_id: i64,
) -> Result<(i64, String, String)> {
    sqlx::query_as("SELECT author_id,state,head_sha FROM pull_requests WHERE id=$1 FOR UPDATE")
        .bind(pr_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))
}

/// Resolving a conversation takes write access, or the pull request's author while they can
/// still read the repository. Both need the live actor, unarchived repository and usable owner
/// that every discussion write needs.
pub(crate) async fn lock_comment_resolution_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    pr_id: i64,
) -> Result<()> {
    let scope: Option<(i64, i64)> =
        sqlx::query_as("SELECT repo_id, author_id FROM pull_requests WHERE id=$1")
            .bind(pr_id)
            .fetch_optional(&mut **tx)
            .await?;
    let (repo, author) = scope.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    anyhow::ensure!(
        lock_discussion_authority(tx, actor_id, repo, RepositoryLock::Shared)
            .await?
            .is_some_and(|a| a.access.can_write()
                || a.personal_override
                || (author == actor_id && a.access.can_read())),
        AUTHORITY_DENIED
    );
    Ok(())
}

pub(crate) async fn lock_review_dismissal_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    review_id: i64,
) -> Result<i64> {
    let scope: Option<(i64, i64)> = sqlx::query_as("SELECT p.id,p.repo_id FROM pr_reviews r JOIN pull_requests p ON p.id=r.pull_request_id WHERE r.id=$1")
        .bind(review_id).fetch_optional(&mut **tx).await?;
    let (pr, repo) = scope.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    anyhow::ensure!(
        lock_discussion_authority(tx, actor_id, repo, RepositoryLock::Shared)
            .await?
            .is_some_and(|a| if a.organization {
                a.access.can_admin()
            } else {
                a.personal_owner || a.personal_override
            }),
        AUTHORITY_DENIED
    );
    let present: Option<i64> =
        sqlx::query_scalar("SELECT id FROM pull_requests WHERE id=$1 AND repo_id=$2 FOR UPDATE")
            .bind(pr)
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?;
    anyhow::ensure!(present.is_some(), AUTHORITY_DENIED);
    Ok(pr)
}

pub async fn list_repo_authors(pool: &sqlx::PgPool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT username FROM users WHERE id IN (
            SELECT author_id FROM issues WHERE repo_id = $1
            UNION SELECT author_id FROM pull_requests WHERE repo_id = $1
        ) ORDER BY username",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}
