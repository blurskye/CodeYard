//! Bounded, durable notification hints shared by web nodes.
use crate::backend::db::{postgres, sqlite, DbPool};

pub const BATCH_SIZE: usize = 256;
const MAX_ACTIVE_TRANSACTIONS: usize = 4096;

pub(crate) struct EventBatch {
    pub high_water: i64,
    pub active: Vec<i64>,
    pub events: Vec<(i64, String)>,
}

/// PostgreSQL uses transaction ids for event ids. Remember transactions still active
/// when advancing the cursor, so a later commit with a smaller id is not skipped.
/// SQLite's single writer already commits ids in order.
async fn poll(pool: &DbPool, cursor: i64, pending: &[i64]) -> Result<EventBatch, sqlx::Error> {
    if pending.len() > MAX_ACTIVE_TRANSACTIONS {
        return Err(sqlx::Error::Protocol(
            "Too many pending notification transactions".into(),
        ));
    }
    let batch = match pool {
        DbPool::Postgres(pool) => {
            postgres::notifications::events::poll(pool, cursor, pending).await?
        }
        DbPool::Sqlite(pool) => sqlite::notifications::events::poll(pool, cursor).await?,
    };
    if batch.active.len() > MAX_ACTIVE_TRANSACTIONS {
        return Err(sqlx::Error::Protocol(
            "Too many active notification transactions".into(),
        ));
    }
    Ok(batch)
}

pub struct EventCursor {
    id: i64,
    pending: Vec<i64>,
}

impl EventCursor {
    /// Existing browser connections reload their inbox before connecting. Start at
    /// the current position, but retain transactions whose hints may commit later.
    pub async fn connect(pool: &DbPool) -> Result<Self, sqlx::Error> {
        let batch = poll(pool, i64::MAX, &[]).await?;
        Ok(Self {
            id: batch.high_water,
            pending: batch.active,
        })
    }

    pub async fn next(&mut self, pool: &DbPool) -> Result<Vec<String>, sqlx::Error> {
        let batch = poll(pool, self.id, &self.pending).await?;
        let full = batch.events.len() == BATCH_SIZE;
        let last = batch.events.last().map_or(0, |(id, _)| *id);
        self.id = self.id.max(last);
        let active: std::collections::HashSet<i64> = batch.active.into_iter().collect();
        // An inactive pending transaction either committed its row or rolled back.
        // A full page may not have reached it yet, so retain it until that page does.
        self.pending
            .retain(|id| active.contains(id) || (full && *id > last));
        self.pending
            .extend(active.into_iter().filter(|id| *id <= self.id));
        self.pending.sort_unstable();
        self.pending.dedup();
        Ok(batch
            .events
            .into_iter()
            .map(|(_, payload)| payload)
            .collect())
    }
}

pub async fn announce(pool: &DbPool, payload: &str) -> Result<(), sqlx::Error> {
    if payload.len() > 4096 {
        return Err(sqlx::Error::Protocol(
            "Notification hint is too large".into(),
        ));
    }
    match pool {
        DbPool::Postgres(pool) => postgres::notifications::events::announce(pool, payload).await,
        DbPool::Sqlite(pool) => sqlite::notifications::events::announce(pool, payload).await,
    }
}

/// Hints are not inbox history. A disconnected browser reloads the authoritative inbox.
pub async fn prune(pool: &DbPool) -> Result<(), sqlx::Error> {
    match pool {
        DbPool::Postgres(pool) => postgres::notifications::events::prune(pool).await,
        DbPool::Sqlite(pool) => sqlite::notifications::events::prune(pool).await,
    }
}
