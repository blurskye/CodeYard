use crate::backend::db::milestones::{
    MilestoneDocumentFacts, MilestoneInput, MilestoneWriteRefusal,
};
use crate::backend::db::models::{Milestone, MilestoneSummary};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

pub async fn milestone_document_facts(
    pool: &SqlitePool,
    repo: i64,
    id: i64,
) -> Result<MilestoneDocumentFacts> {
    Ok(
        sqlx::query_as("SELECT creator_id,updated_at FROM milestones WHERE repo_id=?1 AND id=?2")
            .bind(repo)
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

const MILESTONE_COLUMNS: &str =
    "id, repo_id, number, title, description, due_date, state, created_at, closed_at, creator_id, COALESCE(updated_at, created_at) AS updated_at, COALESCE(due_on, due_date || 'T00:00:00.000000Z') AS due_on";

/// A milestone with its counts, read from the issues and pull requests that name it. A merged
/// pull request is closed.
const SUMMARY_SELECT: &str = "SELECT m.id, m.repo_id, m.number, m.title, m.description, m.due_date, m.state, m.created_at, m.closed_at, m.creator_id, COALESCE(m.updated_at, m.created_at) AS updated_at, COALESCE(m.due_on, m.due_date || 'T00:00:00.000000Z') AS due_on,
    (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.state = 'open')
        + (SELECT COUNT(*) FROM pull_requests p WHERE p.milestone_id = m.id AND p.state = 'open') AS open_items,
    (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.state = 'closed')
        + (SELECT COUNT(*) FROM pull_requests p WHERE p.milestone_id = m.id AND p.state <> 'open') AS closed_items
    FROM milestones m";

async fn begin_write(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Transaction<'_, Sqlite>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor_id, repo_id).await?,
        MilestoneWriteRefusal::Denied
    );
    Ok(tx)
}

/// Titles are unique per repository without regard to case. `except` is the milestone being
/// renamed, which may keep its own title.
async fn ensure_unique_title(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    title: &str,
    except: Option<i32>,
) -> Result<()> {
    let taken: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM milestones WHERE repo_id = ?1 AND lower(title) = lower(?2) AND (?3 IS NULL OR number <> ?3)",
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
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    title: &str,
    description: Option<&str>,
    due_date: Option<&str>,
) -> Result<Milestone> {
    save_milestone(
        pool,
        actor_id,
        repo_id,
        None,
        MilestoneInput {
            title,
            description,
            due_on: crate::backend::db::milestones::due_on(due_date)?,
            state: None,
        },
    )
    .await
}

pub async fn update_milestone(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    number: i32,
    title: &str,
    description: Option<&str>,
    due_date: Option<&str>,
) -> Result<Milestone> {
    save_milestone(
        pool,
        actor_id,
        repo_id,
        Some(number),
        MilestoneInput {
            title,
            description,
            due_on: crate::backend::db::milestones::due_on(due_date)?,
            state: None,
        },
    )
    .await
}

pub async fn save_milestone(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    number: Option<i32>,
    input: MilestoneInput<'_>,
) -> Result<Milestone> {
    crate::backend::issues::validate_milestone(input.title, input.description.unwrap_or(""))
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        input
            .state
            .is_none_or(|state| matches!(state, "open" | "closed")),
        "Invalid milestone state"
    );
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    ensure_unique_title(&mut tx, repo_id, input.title, number).await?;
    let creating = number.is_none();
    let number = match number {
        Some(number) => number,
        None => sqlx::query_scalar::<_, i32>("UPDATE repositories SET milestone_number=milestone_number+1 WHERE id=?1 RETURNING milestone_number").bind(repo_id).fetch_one(&mut *tx).await?,
    };
    let due_date = input.due_on.map(|date| date.format("%Y-%m-%d").to_string());
    let due_on = input
        .due_on
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
    let sql = if creating {
        format!("INSERT INTO milestones (repo_id,number,title,description,due_date,due_on,creator_id,state,updated_at,closed_at) VALUES (?1,?2,?3,?4,?5,?6,?8,COALESCE(?7,'open'),strftime('%Y-%m-%dT%H:%M:%f000Z','now'),CASE WHEN ?7='closed' THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') END) RETURNING {MILESTONE_COLUMNS}")
    } else {
        format!("UPDATE milestones SET title=?3,description=?4,due_date=?5,due_on=?6,state=COALESCE(?7,state),updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now'),closed_at=CASE WHEN ?7='closed' THEN COALESCE(closed_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now')) WHEN ?7='open' THEN NULL ELSE closed_at END WHERE repo_id=?1 AND number=?2 RETURNING {MILESTONE_COLUMNS}")
    };
    let mut query = sqlx::query_as(&sql)
        .bind(repo_id)
        .bind(number)
        .bind(input.title)
        .bind(input.description)
        .bind(due_date)
        .bind(due_on)
        .bind(input.state);
    if creating {
        query = query.bind(actor_id);
    }
    let milestone = query
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(MilestoneWriteRefusal::NotFound)?;
    tx.commit().await?;
    Ok(milestone)
}

pub async fn set_milestone_state(
    pool: &SqlitePool,
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
        r#"UPDATE milestones SET state = ?3, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'),
            closed_at = CASE WHEN ?3 = 'closed' THEN COALESCE(closed_at, strftime('%Y-%m-%dT%H:%M:%f000Z','now')) END
        WHERE repo_id = ?1 AND number = ?2 RETURNING {MILESTONE_COLUMNS}"#
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
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    number: i32,
    expected_id: i64,
) -> Result<()> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let id: i64 = sqlx::query_scalar(
        "SELECT id FROM milestones WHERE repo_id = ?1 AND number = ?2 AND id = ?3",
    )
    .bind(repo_id)
    .bind(number)
    .bind(expected_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(MilestoneWriteRefusal::NotFound)?;
    sqlx::query("UPDATE issues SET milestone_id = NULL WHERE milestone_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE pull_requests SET milestone_id = NULL WHERE milestone_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM milestones WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn get_milestone(
    pool: &SqlitePool,
    repo_id: i64,
    number: i32,
) -> Result<Option<MilestoneSummary>> {
    Ok(sqlx::query_as::<_, MilestoneSummary>(&format!(
        "{SUMMARY_SELECT} WHERE m.repo_id = ?1 AND m.number = ?2"
    ))
    .bind(repo_id)
    .bind(number)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_milestone_by_id(
    pool: &SqlitePool,
    repo_id: i64,
    id: i64,
) -> Result<Option<MilestoneSummary>> {
    Ok(sqlx::query_as::<_, MilestoneSummary>(&format!(
        "{SUMMARY_SELECT} WHERE m.repo_id = ?1 AND m.id = ?2"
    ))
    .bind(repo_id)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_milestones_by_ids(
    pool: &SqlitePool,
    repo_id: i64,
    ids: &[i64],
) -> Result<Vec<MilestoneSummary>> {
    anyhow::ensure!(ids.len() <= 500, "Milestone batch exceeds its limit");
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as::<_, MilestoneSummary>(&format!(
        "{SUMMARY_SELECT} WHERE m.repo_id = ?1 AND m.id IN (SELECT value FROM json_each(?2))"
    ))
    .bind(repo_id)
    .bind(serde_json::to_string(ids)?)
    .fetch_all(pool)
    .await?)
}

pub async fn list_repo_milestones(
    pool: &SqlitePool,
    repo_id: i64,
    state: Option<&str>,
) -> Result<Vec<MilestoneSummary>> {
    Ok(sqlx::query_as::<_, MilestoneSummary>(&format!(
        "{SUMMARY_SELECT} WHERE m.repo_id = ?1 AND (?2 IS NULL OR m.state = ?2) ORDER BY m.due_date IS NULL, m.due_date, m.number"
    ))
    .bind(repo_id)
    .bind(state)
    .fetch_all(pool)
    .await?)
}

pub async fn list_milestones_page(
    pool: &SqlitePool,
    repo_id: i64,
    state: Option<&str>,
    sort: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<MilestoneSummary>> {
    search_milestones_page(pool, repo_id, state, "", sort, limit, offset).await
}

pub async fn search_milestones_page(
    pool: &SqlitePool,
    repo_id: i64,
    state: Option<&str>,
    query: &str,
    sort: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<MilestoneSummary>> {
    let filter = crate::backend::db::milestones::milestone_search_where(false);
    let order = crate::backend::db::milestones::milestone_order(sort);
    Ok(sqlx::query_as(&format!("SELECT * FROM ({SUMMARY_SELECT} WHERE {filter}) summaries ORDER BY {order} LIMIT $4 OFFSET $5")).bind(repo_id).bind(state).bind(query).bind(limit).bind(offset).fetch_all(pool).await?)
}
pub async fn count_matching_milestones(
    pool: &SqlitePool,
    repo_id: i64,
    state: Option<&str>,
    query: &str,
) -> Result<i64> {
    let filter = crate::backend::db::milestones::milestone_search_where(false);
    Ok(
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM milestones m WHERE {filter}"))
            .bind(repo_id)
            .bind(state)
            .bind(query)
            .fetch_one(pool)
            .await?,
    )
}
pub async fn count_milestones(pool: &SqlitePool, repo_id: i64) -> Result<(i64, i64)> {
    Ok(sqlx::query_as("SELECT COUNT(CASE WHEN state='open' THEN 1 END), COUNT(CASE WHEN state='closed' THEN 1 END) FROM milestones WHERE repo_id=?1").bind(repo_id).fetch_one(pool).await?)
}

/// Issue/PR list filters keep counts and dates, but never load description bodies.
pub async fn list_milestone_filters(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Vec<MilestoneSummary>> {
    let select = SUMMARY_SELECT.replace("m.description", "NULL AS description");
    Ok(sqlx::query_as(&format!(
        "{select} WHERE m.repo_id=?1 ORDER BY m.due_date IS NULL, m.due_date, m.number LIMIT 1000"
    ))
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

/// Picker rows need no descriptions or discussion counts.
pub async fn list_milestone_choices(pool: &SqlitePool, repo_id: i64) -> Result<Vec<Milestone>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {} FROM milestones WHERE repo_id=?1 ORDER BY state DESC, due_date IS NULL, due_date, number LIMIT ?2",
        MILESTONE_COLUMNS.replace("description", "NULL AS description")
    ))
    .bind(repo_id)
    .bind(crate::backend::db::milestones::MAX_MILESTONE_CHOICES)
    .fetch_all(pool)
    .await?)
}

pub async fn milestone_choice_by_title(
    pool: &SqlitePool,
    repo_id: i64,
    title: &str,
) -> Result<Option<Milestone>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {} FROM milestones WHERE repo_id=?1 AND title=?2 ORDER BY number LIMIT 1",
        MILESTONE_COLUMNS.replace("description", "NULL AS description")
    ))
    .bind(repo_id)
    .bind(title)
    .fetch_optional(pool)
    .await?)
}

pub async fn milestone_choices_by_ids(
    pool: &SqlitePool,
    repo_id: i64,
    ids: &[i64],
) -> Result<Vec<Milestone>> {
    anyhow::ensure!(ids.len() <= 101, "Milestone page exceeds its limit");
    Ok(sqlx::query_as(&format!(
        "SELECT {} FROM milestones WHERE repo_id=?1 AND id IN (SELECT value FROM json_each(?2))",
        MILESTONE_COLUMNS.replace("description", "NULL AS description")
    ))
    .bind(repo_id)
    .bind(serde_json::to_string(ids)?)
    .fetch_all(pool)
    .await?)
}

pub async fn set_issue_milestone(
    pool: &SqlitePool,
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
    pool: &SqlitePool,
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
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    table: &'static str,
    item_id: i64,
    milestone_number: Option<i32>,
) -> Result<()> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    set_item_milestone_in_transaction(&mut tx, actor_id, repo_id, table, item_id, milestone_number).await?;
    tx.commit().await?;
    Ok(())
}

/// The milestone change of [`set_item_milestone`] inside a transaction that already holds the
/// write authority.
pub(crate) async fn set_item_milestone_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    actor_id: i64,
    repo_id: i64,
    table: &'static str,
    item_id: i64,
    milestone_number: Option<i32>,
) -> Result<()> {
    let item: Option<Option<i64>> = sqlx::query_scalar(&format!(
        "SELECT milestone_id FROM {table} WHERE id = ?1 AND repo_id = ?2"
    ))
    .bind(item_id)
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(item.is_some(), MilestoneWriteRefusal::NotFound);
    let milestone_id = match milestone_number {
        None => None,
        Some(number) => Some(
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM milestones WHERE repo_id = ?1 AND number = ?2",
            )
            .bind(repo_id)
            .bind(number)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(MilestoneWriteRefusal::NotFound)?,
        ),
    };
    let old = item.flatten();
    if old == milestone_id { return Ok(()); }
    let subject = if table == "issues" { "issue" } else { "pull_request" };
    for (id, kind) in [(old, "demilestoned"), (milestone_id, "milestoned")] {
        if let Some(id) = id {
            let title: String = sqlx::query_scalar("SELECT title FROM milestones WHERE id=?1 AND repo_id=?2").bind(id).bind(repo_id).fetch_one(&mut **tx).await?;
            super::timeline::append(tx, repo_id, subject, item_id, actor_id, kind, &title, "").await?;
        }
    }
    sqlx::query(&format!(
        "UPDATE {table} SET milestone_id = ?2 WHERE id = ?1"
    ))
    .bind(item_id)
    .bind(milestone_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
