use crate::notifications::events::EventBatch;
use sqlx::SqlitePool;

pub(crate) async fn poll(pool: &SqlitePool, cursor: i64) -> Result<EventBatch, sqlx::Error> {
    let rows: Vec<(i64, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT h.high_water,e.id,e.payload
         FROM (SELECT COALESCE(MAX(id),0) AS high_water FROM notification_events) h
         LEFT JOIN (SELECT id,payload FROM notification_events
                    WHERE id>?1 ORDER BY id LIMIT 256) e ON TRUE
         ORDER BY e.id",
    )
    .bind(cursor)
    .fetch_all(pool)
    .await?;
    let high_water = rows.first().ok_or(sqlx::Error::RowNotFound)?.0;
    let events = rows
        .into_iter()
        .filter_map(|(_, id, payload)| id.zip(payload).map(|(id, payload)| (id, None, payload)))
        .collect();
    Ok(EventBatch {
        high_water,
        active: Vec::new(),
        events,
    })
}

pub(crate) async fn announce(pool: &SqlitePool, payload: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO notification_events (payload) VALUES (?1)")
        .bind(payload)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn prune(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM notification_events WHERE id IN (
            SELECT id FROM notification_events WHERE created_at<strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')
            ORDER BY created_at,id LIMIT 1024)",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
