use crate::backend::db::notifications::events::EventBatch;
use sqlx::SqlitePool;

pub(crate) async fn poll(pool: &SqlitePool, cursor: i64) -> Result<EventBatch, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let high_water = sqlx::query_scalar("SELECT COALESCE(MAX(id),0) FROM notification_events")
        .fetch_one(&mut *tx)
        .await?;
    let events = sqlx::query_as(
        "SELECT id,payload FROM notification_events WHERE id>?1 ORDER BY id LIMIT 256",
    )
    .bind(cursor)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
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

pub(crate) async fn prune(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM notification_events WHERE id IN (
            SELECT id FROM notification_events WHERE created_at<strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')
            ORDER BY created_at,id LIMIT 1024)",
    )
    .execute(pool)
    .await?;
    Ok(())
}
