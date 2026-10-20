use crate::backend::db::timeline::TimelineEvent;
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

#[allow(clippy::too_many_arguments)]
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
    sqlx::query(
        "INSERT INTO timeline_events (repo_id, subject_kind, subject_id, actor_id, kind, detail, previous) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(repo_id)
    .bind(subject)
    .bind(subject_id)
    .bind(actor_id)
    .bind(kind)
    .bind(detail)
    .bind(previous)
    .execute(pool)
    .await?;
    Ok(())
}

/// The events from `after` up to `before`, each bound optional, oldest first.
pub async fn list_events(
    pool: &SqlitePool,
    subject: &str,
    subject_id: i64,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
    offset: i64,
) -> Result<Vec<TimelineEvent>> {
    anyhow::ensure!(limit >= 0 && offset >= 0, "Invalid timeline page");
    // Stored times are fixed-width UTC text, so the bounds are written the same way.
    let after = after.map(crate::backend::db::sqlite::time::encode);
    let before = before.map(crate::backend::db::sqlite::time::encode);
    Ok(sqlx::query_as(
        r#"
        SELECT u.username AS actor, e.kind, e.detail, e.previous, e.created_at
        FROM timeline_events e LEFT JOIN users u ON u.id = e.actor_id
        WHERE e.subject_kind = ?1 AND e.subject_id = ?2
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
    .fetch_all(pool)
    .await?)
}
