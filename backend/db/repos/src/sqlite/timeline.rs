use crate::assignees::AssignedItem;
use crate::timeline::Event;
use crate::timeline::TimelineEvent;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use sqlx::SqlitePool;

pub async fn get_event(pool: &SqlitePool, repo_id: i64, id: i64) -> Result<Option<Event>> {
    Ok(
        sqlx::query_as("SELECT * FROM timeline_events WHERE id=?1 AND repo_id=?2")
            .bind(id)
            .bind(repo_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_entries(
    pool: &SqlitePool,
    repo_id: i64,
    item: Option<AssignedItem>,
    timeline: bool,
    descending: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<(String, i64)>> {
    let (subject, id) = match item {
        Some(AssignedItem::Issue(id)) => (Some("issue"), Some(id)),
        Some(AssignedItem::PullRequest(id)) => (Some("pull_request"), Some(id)),
        None => (None, None),
    };
    Ok(sqlx::query_as(&crate::timeline::entries_query(descending))
        .bind(repo_id)
        .bind(subject)
        .bind(id)
        .bind(timeline)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one column of the timeline event."
)]
pub async fn record_event(
    pool: &SqlitePool,
    repo_id: i64,
    subject: &str,
    subject_id: i64,
    actor_id: i64,
    kind: &str,
    detail: &str,
    previous: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    append(
        &mut tx, repo_id, subject, subject_id, actor_id, kind, detail, previous,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one column of the timeline event."
)]
pub(crate) async fn append(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    repo_id: i64,
    subject: &str,
    subject_id: i64,
    actor_id: i64,
    kind: &str,
    detail: &str,
    previous: &str,
) -> Result<i64> {
    let (detail, previous) = crate::timeline::event_details(kind, detail, previous)?;
    Ok(sqlx::query_scalar(
        "INSERT INTO timeline_events (repo_id, subject_kind, subject_id, actor_id, kind, detail, previous) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
    )
    .bind(repo_id)
    .bind(subject)
    .bind(subject_id)
    .bind(actor_id)
    .bind(kind)
    .bind(detail)
    .bind(previous)
    .fetch_one(&mut **tx)
    .await?)
}

/// The events from `after` up to `before`, each bound optional, oldest first.
#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one filter or page bound of the query."
)]
pub async fn list_events(
    pool: &SqlitePool,
    repo: i64,
    subject: &str,
    subject_id: i64,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
    offset: i64,
) -> Result<Vec<TimelineEvent>> {
    anyhow::ensure!(limit >= 0 && offset >= 0, "Invalid timeline page");
    // Stored times are fixed-width UTC text, so the bounds are written the same way.
    let after = after.map(codeyard_db_core::queries::sqlite::time::encode);
    let before = before.map(codeyard_db_core::queries::sqlite::time::encode);
    Ok(sqlx::query_as(
        r#"
        SELECT u.username AS actor, e.kind, e.detail, e.previous, e.created_at
        FROM timeline_events e LEFT JOIN users u ON u.id = e.actor_id
        WHERE e.subject_kind = ?1 AND e.subject_id = ?2 AND e.repo_id=?7
          AND ((?1='issue' AND EXISTS(SELECT 1 FROM issues i WHERE i.id=?2 AND i.repo_id=?7))
            OR (?1='pull_request' AND EXISTS(SELECT 1 FROM pull_requests p WHERE p.id=?2 AND p.repo_id=?7)))
          AND (?3 IS NULL OR e.created_at >= ?3) AND (?4 IS NULL OR e.created_at < ?4)
        ORDER BY e.created_at, e.id LIMIT ?5 OFFSET ?6
        "#,
    )
    .bind(subject)
    .bind(subject_id)
    .bind(after)
    .bind(before)
    .bind(limit)
    .bind(offset)
    .bind(repo)
    .fetch_all(pool)
    .await?)
}
