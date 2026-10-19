use crate::backend::db::milestones::MilestoneWriteRefusal;
use crate::backend::db::models::{Milestone, MilestoneSummary};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

// Due dates are stored as midnight UTC of the day, and read back as the day.
const MILESTONE_COLUMNS: &str = "id, repo_id, number, title, description, to_char(due_date AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS due_date, state, created_at, closed_at";

/// A milestone with its issue counts, read from the issues that name it.
const SUMMARY_SELECT: &str = "SELECT m.id, m.repo_id, m.number, m.title, m.description, to_char(m.due_date AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS due_date, m.state, m.created_at, m.closed_at,
    (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.state = 'open')
        + (SELECT COUNT(*) FROM pull_requests p WHERE p.milestone_id = m.id AND p.state = 'open') AS open_items,
    (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.state = 'closed')
        + (SELECT COUNT(*) FROM pull_requests p WHERE p.milestone_id = m.id AND p.state <> 'open') AS closed_items
    FROM milestones m";

/// Every milestone write locks the repository row, so numbers and titles are allocated one
/// writer at a time.
async fn begin_write(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    // The exclusive authority lock takes the membership lock before the repository row, the
    // order every other write uses.
    anyhow::ensure!(
        super::discussions::lock_exclusive_write_authority(&mut tx, actor_id, repo_id).await?,
        MilestoneWriteRefusal::Denied
    );
    Ok(tx)
}

/// Titles are unique per repository without regard to case. `except` is the milestone being
/// renamed, which may keep its own title.
async fn ensure_unique_title(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    title: &str,
    except: Option<i32>,
) -> Result<()> {
    let taken: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM milestones WHERE repo_id = $1 AND LOWER(title) = LOWER($2::TEXT) AND ($3::INTEGER IS NULL OR number <> $3)",
    )
    .bind(repo_id)
    .bind(title)
    .bind(except)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(taken.is_none(), MilestoneWriteRefusal::DuplicateTitle);
    Ok(())
}

pub async fn create_milestone(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    title: &str,
    description: Option<&str>,
    due_date: Option<&str>,
) -> Result<Milestone> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    ensure_unique_title(&mut tx, repo_id, title, None).await?;
    let number: i32 = sqlx::query_scalar(
        "SELECT (COALESCE(MAX(number), 0) + 1)::INTEGER FROM milestones WHERE repo_id = $1",
    )
    .bind(repo_id)
    .fetch_one(&mut *tx)
    .await?;
    let milestone = sqlx::query_as::<_, Milestone>(&format!(
        "INSERT INTO milestones (repo_id, number, title, description, due_date) VALUES ($1, $2, $3, $4, ($5::TEXT::DATE)::TIMESTAMP AT TIME ZONE 'UTC') RETURNING {MILESTONE_COLUMNS}"
    ))
    .bind(repo_id)
    .bind(number)
    .bind(title)
    .bind(description)
    .bind(due_date)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(milestone)
}

pub async fn update_milestone(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    number: i32,
    title: &str,
    description: Option<&str>,
    due_date: Option<&str>,
) -> Result<Milestone> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    ensure_unique_title(&mut tx, repo_id, title, Some(number)).await?;
    let milestone = sqlx::query_as::<_, Milestone>(&format!(
        "UPDATE milestones SET title = $3, description = $4, due_date = ($5::TEXT::DATE)::TIMESTAMP AT TIME ZONE 'UTC' WHERE repo_id = $1 AND number = $2 RETURNING {MILESTONE_COLUMNS}"
    ))
    .bind(repo_id)
    .bind(number)
    .bind(title)
    .bind(description)
    .bind(due_date)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(MilestoneWriteRefusal::NotFound)?;
    tx.commit().await?;
    Ok(milestone)
}

pub async fn set_milestone_state(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    number: i32,
    state: &str,
) -> Result<Milestone> {
    anyhow::ensure!(
        state == "open" || state == "closed",
        "Milestone state must be open or closed"
    );
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let milestone = sqlx::query_as::<_, Milestone>(&format!(
        r#"UPDATE milestones SET state = $3::TEXT,
            closed_at = CASE WHEN $3::TEXT = 'closed' THEN COALESCE(closed_at, NOW()) END
        WHERE repo_id = $1 AND number = $2 RETURNING {MILESTONE_COLUMNS}"#
    ))
    .bind(repo_id)
    .bind(number)
    .bind(state)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(MilestoneWriteRefusal::NotFound)?;
    tx.commit().await?;
    Ok(milestone)
}

pub async fn delete_milestone(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    number: i32,
    expected_id: i64,
) -> Result<()> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let id: i64 = sqlx::query_scalar(
        "SELECT id FROM milestones WHERE repo_id = $1 AND number = $2 AND id = $3",
    )
    .bind(repo_id)
    .bind(number)
    .bind(expected_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(MilestoneWriteRefusal::NotFound)?;
    sqlx::query("UPDATE issues SET milestone_id = NULL WHERE milestone_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE pull_requests SET milestone_id = NULL WHERE milestone_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM milestones WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn get_milestone(
    pool: &PgPool,
    repo_id: i64,
    number: i32,
) -> Result<Option<MilestoneSummary>> {
    Ok(sqlx::query_as::<_, MilestoneSummary>(&format!(
        "{SUMMARY_SELECT} WHERE m.repo_id = $1 AND m.number = $2"
    ))
    .bind(repo_id)
    .bind(number)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_milestone_by_id(
    pool: &PgPool,
    repo_id: i64,
    id: i64,
) -> Result<Option<MilestoneSummary>> {
    Ok(sqlx::query_as::<_, MilestoneSummary>(&format!(
        "{SUMMARY_SELECT} WHERE m.repo_id = $1 AND m.id = $2"
    ))
    .bind(repo_id)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_repo_milestones(
    pool: &PgPool,
    repo_id: i64,
    state: Option<&str>,
) -> Result<Vec<MilestoneSummary>> {
    Ok(sqlx::query_as::<_, MilestoneSummary>(&format!(
        "{SUMMARY_SELECT} WHERE m.repo_id = $1 AND ($2::TEXT IS NULL OR m.state = $2) ORDER BY m.due_date IS NULL, m.due_date, m.number"
    ))
    .bind(repo_id)
    .bind(state)
    .fetch_all(pool)
    .await?)
}

pub async fn set_issue_milestone(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    milestone_number: Option<i32>,
) -> Result<()> {
    set_item_milestone(
        pool,
        actor_id,
        repo_id,
        "issues",
        issue_id,
        milestone_number,
    )
    .await
}

pub async fn set_pull_request_milestone(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    pull_request_id: i64,
    milestone_number: Option<i32>,
) -> Result<()> {
    set_item_milestone(
        pool,
        actor_id,
        repo_id,
        "pull_requests",
        pull_request_id,
        milestone_number,
    )
    .await
}

/// Sets the milestone of an issue or a pull request. `table` is the item's table, named by the
/// callers in this module.
async fn set_item_milestone(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    table: &'static str,
    item_id: i64,
    milestone_number: Option<i32>,
) -> Result<()> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let item: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id = $1 AND repo_id = $2"
    ))
    .bind(item_id)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(item.is_some(), MilestoneWriteRefusal::NotFound);
    let milestone_id = match milestone_number {
        None => None,
        Some(number) => Some(
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM milestones WHERE repo_id = $1 AND number = $2",
            )
            .bind(repo_id)
            .bind(number)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(MilestoneWriteRefusal::NotFound)?,
        ),
    };
    sqlx::query(&format!(
        "UPDATE {table} SET milestone_id = $2 WHERE id = $1"
    ))
    .bind(item_id)
    .bind(milestone_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
