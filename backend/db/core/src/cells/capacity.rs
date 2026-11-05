//! A bounded native-cell sample; counters and bytes never read another cell's schema.
use anyhow::{ensure, Result};

#[derive(Clone, Copy)]
pub struct Sample {
    pub database_bytes: i64,
    pub writes: u64,
}

pub async fn sample(pool: &crate::pool::PgPool, maximum_tables: usize) -> Result<Sample> {
    ensure!(
        (1..=1024).contains(&maximum_tables),
        "Invalid cell table limit"
    );
    let rows: Vec<(i64, i64, i64, i64)> = sqlx::query_as("SELECT pg_total_relation_size(relid),n_tup_ins,n_tup_upd,n_tup_del FROM pg_stat_user_tables WHERE schemaname=current_schema() ORDER BY relid LIMIT $1")
        .bind((maximum_tables + 1) as i64).fetch_all(pool).await?;
    ensure!(
        rows.len() <= maximum_tables,
        "Cell table catalog exceeds its limit"
    );
    let mut database_bytes = 0i64;
    let mut writes = 0u64;
    for (bytes, inserted, updated, deleted) in rows {
        ensure!(
            [bytes, inserted, updated, deleted]
                .iter()
                .all(|value| *value >= 0),
            "Invalid cell capacity sample"
        );
        database_bytes = database_bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("Cell size exceeds its limit"))?;
        for value in [inserted, updated, deleted] {
            writes = writes.saturating_add(value as u64);
        }
    }
    Ok(Sample {
        database_bytes,
        writes,
    })
}
