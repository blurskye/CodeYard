use crate::backend::db::assignees::AssignedItem;
use crate::backend::db::timeline::{Event, TimelineEvent};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

pub async fn get_event(pool: &PgPool, repo_id: i64, id: i64) -> Result<Option<Event>> {
    Ok(
        sqlx::query_as("SELECT * FROM timeline_events WHERE id=$1 AND repo_id=$2")
            .bind(id)
            .bind(repo_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_entries(
    pool: &PgPool,
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
    Ok(
        sqlx::query_as(&crate::backend::db::timeline::entries_query(descending))
            .bind(repo_id)
            .bind(subject)
            .bind(id)
            .bind(timeline)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await?,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn record_event(
    pool: &PgPool,
    repo_id: i64,
    subject: &str,
    subject_id: i64,
    actor_id: i64,
    kind: &str,
    detail: &str,
    previous: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO timeline_events (repo_id, subject_kind, subject_id, actor_id, kind, detail, previous) VALUES ($1, $2, $3, $4, $5, $6, $7)",
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
    pool: &PgPool,
    subject: &str,
    subject_id: i64,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
    offset: i64,
) -> Result<Vec<TimelineEvent>> {
    anyhow::ensure!(limit >= 0 && offset >= 0, "Invalid timeline page");
    Ok(sqlx::query_as(
        r#"
        SELECT u.username::text AS actor, e.kind::text AS kind, e.detail::text AS detail,
               e.previous::text AS previous, e.created_at
        FROM timeline_events e LEFT JOIN users u ON u.id = e.actor_id
        WHERE e.subject_kind = $1 AND e.subject_id = $2
          AND ($3::timestamptz IS NULL OR e.created_at >= $3)
          AND ($4::timestamptz IS NULL OR e.created_at < $4)
        ORDER BY e.created_at, e.id LIMIT $5 OFFSET $6
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
