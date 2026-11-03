use crate::backend::db::notifications::events::EventBatch;
use sqlx::PgPool;

pub(crate) async fn poll(
    pool: &PgPool,
    cursor: i64,
    pending: &[i64],
) -> Result<EventBatch, sqlx::Error> {
    // The snapshot and the rows must come from one statement. A separate snapshot
    // could miss a transaction that starts and commits between the two queries.
    let (high_water, active, ids, payloads): (i64, Vec<i64>, Vec<i64>, Vec<String>) =
        sqlx::query_as(
            "WITH snapshot AS MATERIALIZED (SELECT pg_current_snapshot() AS value),
         events AS MATERIALIZED (
             SELECT id,payload FROM (
                 (SELECT id,payload FROM notification_events WHERE id>$1 ORDER BY id LIMIT 256)
                 UNION ALL
                 (SELECT id,payload FROM notification_events WHERE id<=$1 AND id=ANY($2))
             ) candidates ORDER BY id LIMIT 256
         )
         SELECT pg_snapshot_xmax(value)::text::bigint-1,
                ARRAY(SELECT xid::text::bigint FROM pg_snapshot_xip(value) xid LIMIT 4097),
                ARRAY(SELECT id FROM events ORDER BY id),
                ARRAY(SELECT payload FROM events ORDER BY id)
         FROM snapshot",
        )
        .bind(cursor)
        .bind(pending)
        .fetch_one(pool)
        .await?;
    Ok(EventBatch {
        high_water,
        active,
        events: ids.into_iter().zip(payloads).collect(),
    })
}

pub(crate) async fn announce(pool: &PgPool, payload: &str) -> Result<(), sqlx::Error> {
    // Exactly one hint per standalone transaction; its xid is unique and ordered
    // without a sequence or a shared row lock at commit.
    sqlx::query("INSERT INTO notification_events (payload) VALUES ($1)")
        .bind(payload)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn prune(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM notification_events WHERE id IN (
            SELECT id FROM notification_events WHERE created_at<clock_timestamp()-INTERVAL '1 hour'
            ORDER BY created_at,id LIMIT 1024)",
    )
    .execute(pool)
    .await?;
    Ok(())
}
