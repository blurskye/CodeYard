//! PostgreSQL schema migrations.

use crate::pool::PgPool;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tracing::info;

const MIGRATIONS: &[(i64, &str, &str)] = &[(
    1,
    "schema",
    include_str!("../../../../../schema/postgres/0001.schema.sql"),
)];

static MIGRATION_TURNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    let mut connection = crate::connection::session_connection(pool).await?;
    let applied = apply(&mut connection, MIGRATIONS).await;
    let _ = sqlx::Connection::close(connection).await;
    applied
}

/// Independent stores use the same transactional baseline and history checks.
pub async fn run_baseline(pool: &PgPool, name: &'static str, sql: &'static str) -> Result<()> {
    let mut connection = pool.acquire().await?;
    apply(&mut *connection, &[(1, name, sql)]).await
}

async fn apply(
    connection: &mut sqlx::PgConnection,
    migrations: &[(i64, &str, &str)],
) -> Result<()> {
    info!("Running database migrations...");

    // Runs in this process take turns. Otherwise a queue of them, such as many test
    // fixtures starting at once, waits on the database lock below until its timeout.
    let _turn = MIGRATION_TURNS.lock().await;
    // A transaction-scoped advisory lock: concurrent nodes migrate one at a time.
    const MIGRATION_LOCK_KEY: i64 = 37_000_001;
    let mut tx = sqlx::Connection::begin(connection).await?;
    sqlx::query("SET LOCAL lock_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '120s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _gitserv_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, sha256 TEXT NOT NULL, applied_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp())",
    ).execute(&mut *tx).await?;
    let applied: Vec<(i64, String)> =
        sqlx::query_as("SELECT version, sha256 FROM _gitserv_migrations ORDER BY version")
            .fetch_all(&mut *tx)
            .await?;
    validate_history_for(&applied, migrations)?;
    for &(version, name, sql) in &migrations[applied.len()..] {
        sqlx::Executor::execute(&mut *tx, sqlx::raw_sql(sql))
            .await
            .with_context(|| format!("apply database migration {version} ({name})"))?;
        sqlx::query("INSERT INTO _gitserv_migrations (version, name, sha256) VALUES ($1, $2, $3)")
            .bind(version)
            .bind(name)
            .bind(format!("{:x}", Sha256::digest(sql.as_bytes())))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    info!("Migrations completed successfully");
    Ok(())
}

pub fn validate_history(applied: &[(i64, String)]) -> Result<()> {
    validate_history_for(applied, MIGRATIONS)
}

fn validate_history_for(applied: &[(i64, String)], migrations: &[(i64, &str, &str)]) -> Result<()> {
    for (index, (version, checksum)) in applied.iter().enumerate() {
        let Some((expected_version, _, sql)) = migrations.get(index) else {
            bail!("database has migrations newer than this application; downgrade refused");
        };
        if version != expected_version
            || checksum != &format!("{:x}", Sha256::digest(sql.as_bytes()))
        {
            bail!("unexpected baseline history/checksum mismatch for migration {version}: the recorded history differs from this build; no changes applied");
        }
    }
    Ok(())
}
