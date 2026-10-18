//! SQLite schema migrations.
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

const MIGRATIONS: &[(i64, &str, &str)] = &[(
    1,
    "schema",
    include_str!("../../../../schema/sqlite/0001.schema.sql"),
)];

pub async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS _gitserv_sqlite_migrations(version INTEGER PRIMARY KEY,name TEXT NOT NULL,sha256 TEXT NOT NULL,applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')))")
        .execute(&mut *tx).await?;
    let applied: Vec<(i64, String)> =
        sqlx::query_as("SELECT version,sha256 FROM _gitserv_sqlite_migrations ORDER BY version")
            .fetch_all(&mut *tx)
            .await?;
    validate_history(&applied)?;
    // With nothing pending, skip the foreign key scan: it holds the writer lock for the
    // whole database, and ordinary commands such as `user list` run on every start.
    if applied.len() == MIGRATIONS.len() {
        tx.commit().await?;
        return Ok(());
    }
    for &(version, name, sql) in &MIGRATIONS[applied.len()..] {
        // A populated, unversioned/foreign database is NOT silently adopted: native
        // CREATE statements refuse it and the whole migration transaction rolls back.
        sqlx::Executor::execute(&mut *tx, sqlx::raw_sql(sql))
            .await
            .with_context(|| format!("apply native SQLite migration {version}"))?;
        sqlx::query(
            "INSERT INTO _gitserv_sqlite_migrations(version,name,sha256) VALUES (?1,?2,?3)",
        )
        .bind(version)
        .bind(name)
        .bind(format!("{:x}", Sha256::digest(sql.as_bytes())))
        .execute(&mut *tx)
        .await?;
    }
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut *tx)
        .await?;
    if !violations.is_empty() {
        bail!("SQLite foreign key validation failed");
    }
    tx.commit().await?;
    Ok(())
}

pub(in crate::backend::db) fn validate_history(applied: &[(i64, String)]) -> Result<()> {
    for (index, (version, checksum)) in applied.iter().enumerate() {
        let Some((expected, _, sql)) = MIGRATIONS.get(index) else {
            bail!("SQLite database history is newer than this application; downgrade refused");
        };
        if version != expected || checksum != &format!("{:x}", Sha256::digest(sql.as_bytes())) {
            bail!("unexpected SQLite baseline history/checksum mismatch: the recorded history differs from this build; no changes applied");
        }
    }
    Ok(())
}
