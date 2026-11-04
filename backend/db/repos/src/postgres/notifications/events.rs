use crate::notifications::events::EventBatch;
use sqlx::PgPool;

/// The high-water mark, the active transactions, and the event ids, transactions and payloads.
type PollRow = (i64, Vec<i64>, Vec<i64>, Vec<Option<i64>>, Vec<String>);

pub(crate) async fn poll(
    pool: &PgPool,
    cursor: i64,
    pending: &[i64],
) -> Result<EventBatch, sqlx::Error> {
    // The snapshot and the rows must come from one statement. A separate snapshot
    // could miss a transaction that starts and commits between the two queries.
    let (high_water, active, ids, transactions, payloads): PollRow =
        sqlx::query_as(
            "WITH snapshot AS MATERIALIZED (SELECT pg_current_snapshot() AS value),
         events AS MATERIALIZED (
             SELECT id,transaction_id,payload FROM (
                 (SELECT id,transaction_id,payload FROM notification_events WHERE id>$1 ORDER BY id LIMIT 256)
                 UNION ALL
                 (SELECT id,transaction_id,payload FROM notification_events
                  WHERE id<=$1 AND transaction_id=ANY($2) ORDER BY id LIMIT 256)
             ) candidates ORDER BY id LIMIT 256
         )
         SELECT (SELECT COALESCE(MAX(id),0) FROM notification_events),
                ARRAY(SELECT xid::text::bigint FROM pg_snapshot_xip(value) xid LIMIT 4097),
                ARRAY(SELECT id FROM events ORDER BY id),
                ARRAY(SELECT transaction_id FROM events ORDER BY id),
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
        events: ids
            .into_iter()
            .zip(transactions)
            .zip(payloads)
            .map(|((id, transaction), payload)| (id, transaction, payload))
            .collect(),
    })
}

pub(crate) async fn announce(pool: &PgPool, payload: &str) -> Result<(), sqlx::Error> {
    // One hint per standalone transaction lets the cursor retire each pending
    // transaction as soon as its row is delivered, without a shared commit lock.
    // Assign the transaction id before allocating the event id, so a snapshot
    // cannot advance past an event whose writer is not in its active list yet.
    sqlx::query(
        "WITH writer AS MATERIALIZED (SELECT pg_current_xact_id()::text::bigint AS id)
         INSERT INTO notification_events (transaction_id,payload) SELECT id,$1 FROM writer",
    )
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn prune(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM notification_events WHERE id IN (
            SELECT id FROM notification_events WHERE created_at<clock_timestamp()-INTERVAL '1 hour'
            ORDER BY created_at,id LIMIT 1024)",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
