use crate::assignees::AssignmentRefusal;
use crate::issues::management::LinkedBranch;
use crate::issues::management::Refusal;
use anyhow::ensure;
use anyhow::Result;
use codeyard_database::models::Issue;
use codeyard_database::pool::PgPool;
use codeyard_database::queries::credentials::Actor;
use codeyard_types::credentials::AccountRestricted;
use sqlx::Postgres;
use sqlx::Transaction;

/// Pull requests the viewer can read whose body may close issue `number` of `repo`: same-repository
/// ones mentioning the number, and others naming `scope` (`owner/name`). Only bodies that hold the
/// number as `#N` or `/issues/N`, the forms a closing reference takes, fill the limit. Callers
/// parse the bodies.
pub async fn closing_candidates(
    pool: &PgPool,
    viewer: Option<i64>,
    repo: i64,
    scope: &str,
    number: i32,
    before: i64,
) -> Result<Vec<codeyard_database::models::PullRequest>> {
    Ok(sqlx::query_as(&format!(
        "SELECT p.* FROM pull_requests p JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id=$1
         WHERE (p.body LIKE $2 OR p.body LIKE $5) AND (p.repo_id=$3 OR p.body ILIKE $4) AND p.base_branch=r.default_branch AND ($6<=0 OR p.id<$6) AND {}
         ORDER BY p.id DESC LIMIT 100",
        codeyard_database::queries::postgres::repositories::access::readable_condition()
    ))
    .bind(viewer)
    .bind(format!("%#{number}%"))
    .bind(repo)
    .bind(format!("%{scope}%"))
    .bind(format!("%/issues/{number}%"))
    .bind(before)
    .fetch_all(pool)
    .await?)
}

pub async fn duplicate(pool: &PgPool, issue: i64) -> Result<Option<Issue>> {
    Ok(sqlx::query_as("SELECT i.* FROM issue_duplicates d JOIN issues i ON i.id=d.duplicate_id WHERE d.issue_id=$1").bind(issue).fetch_optional(pool).await?)
}

pub async fn pinned(pool: &PgPool, repo: i64) -> Result<Vec<Issue>> {
    Ok(sqlx::query_as("SELECT i.* FROM pinned_issues p JOIN issues i ON i.id=p.issue_id AND i.repo_id=p.repo_id WHERE p.repo_id=$1 ORDER BY p.slot").bind(repo).fetch_all(pool).await?)
}

async fn require_actor(tx: &mut Transaction<'_, Postgres>, actor: &Actor) -> Result<()> {
    codeyard_db_identity::postgres::credentials::require_account(tx, actor)
        .await
        .map_err(|error| {
            if error.is::<AccountRestricted>() {
                Refusal::Denied.into()
            } else {
                error
            }
        })
}

async fn issue(tx: &mut Transaction<'_, Postgres>, repo: i64, id: i64) -> Result<Issue> {
    sqlx::query_as("SELECT * FROM issues WHERE id=$1 AND repo_id=$2 FOR UPDATE")
        .bind(id)
        .bind(repo)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Refusal::Missing.into())
}

pub async fn pin(
    pool: &PgPool,
    actor: impl Into<Actor>,
    repo: i64,
    id: i64,
    pinned: bool,
) -> Result<()> {
    let authority = actor.into();
    let actor = authority.user_id;
    let mut tx = pool.begin().await?;
    ensure!(
        crate::postgres::discussions::lock_exclusive_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    require_actor(&mut tx, &authority).await?;
    issue(&mut tx, repo, id).await?;
    if pinned {
        let slots: Vec<i32> = sqlx::query_scalar("SELECT slot FROM pinned_issues WHERE repo_id=$1")
            .bind(repo)
            .fetch_all(&mut *tx)
            .await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pinned_issues WHERE issue_id=$1)")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            let slot = (1..=3)
                .find(|slot| !slots.contains(slot))
                .ok_or(Refusal::PinLimit)?;
            sqlx::query("INSERT INTO pinned_issues(repo_id,issue_id,slot) VALUES($1,$2,$3)")
                .bind(repo)
                .bind(id)
                .bind(slot)
                .execute(&mut *tx)
                .await?;
        }
    } else {
        sqlx::query("DELETE FROM pinned_issues WHERE repo_id=$1 AND issue_id=$2")
            .bind(repo)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, actor: impl Into<Actor>, repo: i64, id: i64) -> Result<()> {
    let authority = actor.into();
    let actor = authority.user_id;
    let mut tx = pool.begin().await?;
    ensure!(
        codeyard_database::queries::postgres::discussions::lock_admin_authority(
            &mut tx, actor, repo
        )
        .await?,
        Refusal::Denied
    );
    require_actor(&mut tx, &authority).await?;
    issue(&mut tx, repo, id).await?;
    sqlx::query("DELETE FROM reactions WHERE subject_kind='issue' AND subject_id=$1 OR subject_kind='issue_comment' AND subject_id IN (SELECT id FROM issue_comments WHERE issue_id=$1)").bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM notifications WHERE repo_id=$1 AND kind IN ('issue_comment','mention') AND metadata->>'comment_id' IN (SELECT id::text FROM issue_comments WHERE issue_id=$2)")
        .bind(repo)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM timeline_events WHERE subject_kind='issue' AND subject_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM issues WHERE id=$1 AND repo_id=$2")
        .bind(id)
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn transfer(
    pool: &PgPool,
    actor: impl Into<Actor>,
    repo: i64,
    id: i64,
    destination: i64,
) -> Result<Issue> {
    ensure!(repo != destination, Refusal::Destination);
    let authority = actor.into();
    let actor = authority.user_id;
    let mut tx = pool.begin().await?;
    for repo in [repo.min(destination), repo.max(destination)] {
        ensure!(
            crate::postgres::discussions::lock_exclusive_write_authority(&mut tx, actor, repo)
                .await?,
            Refusal::Denied
        );
    }
    require_actor(&mut tx, &authority).await?;
    let same_owner: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories a JOIN repositories b ON a.owner_user_id=b.owner_user_id OR a.owner_org_id=b.owner_org_id WHERE a.id=$1 AND b.id=$2)").bind(repo).bind(destination).fetch_one(&mut *tx).await?;
    ensure!(same_owner, Refusal::Destination);
    // A private discussion must not become public through a transfer.
    let publishes: bool = sqlx::query_scalar("SELECT a.is_private AND NOT b.is_private FROM repositories a JOIN repositories b ON b.id=$2 WHERE a.id=$1")
        .bind(repo)
        .bind(destination)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(!publishes, Refusal::Destination);
    let old = issue(&mut tx, repo, id).await?;
    let number: i32 = sqlx::query_scalar("SELECT get_next_issue_number($1)")
        .bind(destination)
        .fetch_one(&mut *tx)
        .await?;
    let labels: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT b.id FROM issue_labels il JOIN labels a ON a.id=il.label_id JOIN labels b ON lower(b.name)=lower(a.name) WHERE il.issue_id=$1 AND b.repo_id=$2").bind(id).bind(destination).fetch_all(&mut *tx).await?;
    sqlx::query("DELETE FROM issue_labels WHERE issue_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for label in labels {
        sqlx::query("INSERT INTO issue_labels(issue_id,label_id) VALUES($1,$2)")
            .bind(id)
            .bind(label)
            .execute(&mut *tx)
            .await?;
    }
    let milestone: Option<i64> = sqlx::query_scalar("SELECT b.id FROM milestones a JOIN milestones b ON lower(b.title)=lower(a.title) WHERE a.id=$1 AND b.repo_id=$2 ORDER BY b.id LIMIT 1").bind(old.milestone_id).bind(destination).fetch_optional(&mut *tx).await?;
    sqlx::query("DELETE FROM pinned_issues WHERE issue_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO issue_redirects(repo_id,number,issue_id) VALUES($1,$2,$3)")
        .bind(repo)
        .bind(old.number)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE timeline_events SET repo_id=$1 WHERE subject_kind='issue' AND subject_id=$2",
    )
    .bind(destination)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE reactions SET repo_id=$1 WHERE (subject_kind='issue' AND subject_id=$2) OR (subject_kind='issue_comment' AND subject_id IN (SELECT id FROM issue_comments WHERE issue_id=$2))")
        .bind(destination)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Both repositories share an owner, so the issue stays in that owner's projects.
    sqlx::query("UPDATE project_items SET repo_id=$1 WHERE issue_id=$2")
        .bind(destination)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Assignees who cannot be assigned in the destination would be left on a task they cannot see.
    let assignees: Vec<i64> =
        sqlx::query_scalar("SELECT user_id FROM issue_assignees WHERE issue_id=$1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    for user in assignees {
        match crate::postgres::assignees::candidate(&mut tx, destination, user).await {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.downcast_ref::<AssignmentRefusal>(),
                    Some(AssignmentRefusal::Unassignable)
                ) =>
            {
                sqlx::query("DELETE FROM issue_assignees WHERE issue_id=$1 AND user_id=$2")
                    .bind(id)
                    .bind(user)
                    .execute(&mut *tx)
                    .await?;
            }
            Err(error) => return Err(error),
        }
    }
    let result = sqlx::query_as(
        "UPDATE issues SET repo_id=$1,number=$2,milestone_id=$3,updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$4 RETURNING *",
    )
    .bind(destination)
    .bind(number)
    .bind(milestone)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn redirect(pool: &PgPool, repo: i64, number: i32) -> Result<Option<Issue>> {
    Ok(sqlx::query_as("SELECT i.* FROM issue_redirects r JOIN issues i ON i.id=r.issue_id WHERE r.repo_id=$1 AND r.number=$2").bind(repo).bind(number).fetch_optional(pool).await?)
}

pub async fn close_duplicate_with_previous(
    pool: &PgPool,
    actor: impl Into<Actor>,
    repo: i64,
    id: i64,
    duplicate: i64,
) -> Result<(Issue, Issue)> {
    ensure!(id != duplicate, Refusal::Duplicate);
    let authority = actor.into();
    let actor = authority.user_id;
    let mut tx = pool.begin().await?;
    let target_repo: i64 = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id=$1")
        .bind(duplicate)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    for current in [repo.min(target_repo), repo.max(target_repo)] {
        if current == repo {
            ensure!(
                crate::postgres::discussions::lock_exclusive_write_authority(&mut tx, actor, repo)
                    .await?,
                Refusal::Denied
            );
        } else {
            let access = codeyard_database::queries::postgres::repositories::access::lock_user_repo_access_in_transaction(
                &mut tx,
                actor,
                current,
                codeyard_database::queries::postgres::repositories::access::RepositoryRowLock::Shared,
                true,
            )
            .await?;
            ensure!(
                access.is_some_and(|a| a.access.can_read() || (!a.organization && a.is_admin)),
                Refusal::Missing
            );
        }
    }
    require_actor(&mut tx, &authority).await?;
    let previous = issue(&mut tx, repo, id).await?;
    issue(&mut tx, target_repo, duplicate).await?;
    sqlx::query("INSERT INTO issue_duplicates(issue_id,duplicate_id) VALUES($1,$2) ON CONFLICT(issue_id) DO UPDATE SET duplicate_id=excluded.duplicate_id").bind(id).bind(duplicate).execute(&mut *tx).await?;
    let result: Issue = sqlx::query_as("UPDATE issues SET state='closed',state_reason='duplicate',closed_by_id=CASE WHEN state='open' THEN $1 ELSE closed_by_id END,closed_at=COALESCE(closed_at,clock_timestamp()),updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$2 RETURNING *").bind(actor).bind(id).fetch_one(&mut *tx).await?;
    for (kind, detail, old) in crate::timeline::edit_events(
        &previous.title,
        &previous.state,
        &result.title,
        &result.state,
    ) {
        crate::postgres::timeline::append(&mut tx, repo, "issue", id, actor, kind, detail, old)
            .await?;
    }
    tx.commit().await?;
    Ok((previous, result))
}

pub async fn linked_branches(pool: &PgPool, issue: i64) -> Result<Vec<LinkedBranch>> {
    Ok(sqlx::query_as(
        "SELECT id,repo_id,name FROM issue_linked_branches WHERE issue_id=$1 ORDER BY id",
    )
    .bind(issue)
    .fetch_all(pool)
    .await?)
}

pub async fn link_branch(
    pool: &PgPool,
    actor: impl Into<Actor>,
    repo: i64,
    id: i64,
    branch_repo: i64,
    name: &str,
) -> Result<LinkedBranch> {
    let authority = actor.into();
    let actor = authority.user_id;
    let mut tx = pool.begin().await?;
    for repo in [repo.min(branch_repo), repo.max(branch_repo)] {
        ensure!(
            crate::postgres::discussions::lock_exclusive_write_authority(&mut tx, actor, repo)
                .await?,
            Refusal::Denied
        );
    }
    require_actor(&mut tx, &authority).await?;
    issue(&mut tx, repo, id).await?;
    let result = sqlx::query_as("INSERT INTO issue_linked_branches(issue_id,repo_id,name) VALUES($1,$2,$3) ON CONFLICT(issue_id,repo_id,name) DO UPDATE SET name=excluded.name RETURNING id,repo_id,name").bind(id).bind(branch_repo).bind(name).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}
