//! Database notification hints. The broadcaster owns serialization, filtering and
//! its receive task; this resource owns only the PostgreSQL listener connection.
use crate::backend::db::{postgres, DbPool};

pub struct NotificationListener {
    native: postgres::notifications::events::NotificationListener,
}

impl NotificationListener {
    /// SQLite delivers locally and has no remote listener.
    pub async fn connect(pool: &DbPool) -> Result<Option<Self>, sqlx::Error> {
        match pool {
            DbPool::Postgres(pool) => Ok(Some(Self {
                native: postgres::notifications::events::NotificationListener::connect(pool)
                    .await?,
            })),
            DbPool::Sqlite(_) => Ok(None),
        }
    }

    /// PostgreSQL reconnects after connection loss. Hints missed during an outage
    /// are not replayed; recipients reload authoritative rows on reconnect.
    pub async fn recv(&mut self) -> Result<String, sqlx::Error> {
        self.native.recv().await
    }
}

/// Announces the existing serialized `{origin,event}` payload after record commit.
/// SQLite uses the broadcaster's local channel instead.
pub async fn announce(pool: &DbPool, payload: &str) -> Result<(), sqlx::Error> {
    match pool {
        DbPool::Postgres(pool) => postgres::notifications::events::announce(pool, payload).await,
        DbPool::Sqlite(_) => Ok(()),
    }
}
