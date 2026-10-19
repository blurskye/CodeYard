//! PostgreSQL schema migrations.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::info;

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "schema",
        include_str!("../../../../schema/postgres/0001.schema.sql"),
    ),
    (
        2,
        "account_invitations",
        include_str!("../../../../schema/postgres/0002.account_invitations.sql"),
    ),
    (
        3,
        "releases",
        include_str!("../../../../schema/postgres/0003.releases.sql"),
    ),
    (
        4,
        "webhooks",
        include_str!("../../../../schema/postgres/0004.webhooks.sql"),
    ),
    (
        5,
        "two_factor",
        include_str!("../../../../schema/postgres/0005.two_factor.sql"),
    ),
    (
        6,
        "deploy_keys",
        include_str!("../../../../schema/postgres/0006.deploy_keys.sql"),
    ),
    (
        7,
        "wiki",
        include_str!("../../../../schema/postgres/0007.wiki.sql"),
    ),
    (
        8,
        "codeowners_rule",
        include_str!("../../../../schema/postgres/0008.codeowners_rule.sql"),
    ),
];

static MIGRATION_TURNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    info!("Running database migrations...");

    // Runs in this process take turns. Otherwise a queue of them, such as many test
    // fixtures starting at once, waits on the database lock below until its timeout.
    let _turn = MIGRATION_TURNS.lock().await;
    // A transaction-scoped advisory lock: concurrent nodes migrate one at a time.
    const MIGRATION_LOCK_KEY: i64 = 37_000_001;
    let mut tx = pool.begin().await?;
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
    validate_history(&applied)?;
    for &(version, name, sql) in &MIGRATIONS[applied.len()..] {
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

pub(in crate::backend::db) fn validate_history(applied: &[(i64, String)]) -> Result<()> {
    for (index, (version, checksum)) in applied.iter().enumerate() {
        let Some((expected_version, _, sql)) = MIGRATIONS.get(index) else {
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
