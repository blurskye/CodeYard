//! Schema migrations, one baseline per database engine.
use super::DbPool;
use anyhow::Result;

pub async fn run_migrations(pool: &DbPool) -> Result<()> {
    match pool {
        DbPool::Postgres(native) => super::postgres::migrations::run_migrations(native).await,
        DbPool::Sqlite(native) => super::sqlite::migrations::run_migrations(native).await,
    }
}
