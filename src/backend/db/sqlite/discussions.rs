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
        "SELECT owner_kind, owner_user_id, owner_org_id FROM repositories WHERE id = ?1 AND (?2 OR NOT is_archived)",
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
    let access =
        super::repositories::access::user_repo_access_in_transaction(tx, actor_id, repo_id).await?;
    Ok(Some(DiscussionAuthority {
        access,
        personal_override: !organization && admin,
        personal_owner: !organization && owner == Some(actor_id),
        organization,
    }))
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

/// Administration of archival state must also work when the repository is already archived.
pub(crate) async fn lock_archive_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, true)
        .await?
        .is_some_and(|a| a.access.can_admin() || a.personal_owner || a.personal_override))
}

/// Whether the actor may still take part in the repository (read access is enough).
pub(crate) async fn lock_contribution_authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(lock_discussion_authority(tx, actor_id, repo_id, false)
        .await?
        .is_some_and(|a| a.access.can_read() || a.personal_override))
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

pub(crate) async fn lock_issue_contribution(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    issue_id: i64,
) -> Result<()> {
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id=?1")
        .bind(issue_id)
        .fetch_optional(&mut **tx)
        .await?;
    let repo = repo.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    require_contribution_authority(tx, actor_id, repo).await?;
    let locked: Option<bool> =
        sqlx::query_scalar("SELECT locked FROM issues WHERE id=?1 AND repo_id=?2")
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
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    pr_id: i64,
) -> Result<()> {
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=?1")
        .bind(pr_id)
        .fetch_optional(&mut **tx)
        .await?;
    let repo = repo.ok_or_else(|| anyhow::anyhow!(AUTHORITY_DENIED))?;
    require_contribution_authority(tx, actor_id, repo).await?;
    let present: Option<i64> =
        sqlx::query_scalar("SELECT id FROM pull_requests WHERE id=?1 AND repo_id=?2")
            .bind(pr_id)
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?;
    anyhow::ensure!(present.is_some(), AUTHORITY_DENIED);
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

pub async fn list_repo_authors(pool: &SqlitePool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT username FROM users WHERE id IN (
            SELECT author_id FROM issues WHERE repo_id = ?1
            UNION SELECT author_id FROM pull_requests WHERE repo_id = ?1
        ) ORDER BY username",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}
