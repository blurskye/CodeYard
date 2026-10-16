//! PostgreSQL schema migrations.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::info;

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "baseline",
        include_str!("../../../../schema/postgres/schema.sql"),
    ),
    (
        2,
        "CI context names",
        include_str!("../../../../schema/postgres/002-ci-contexts.sql"),
    ),
    (
        3,
        "CI pull request identity",
        include_str!("../../../../schema/postgres/003-ci-pull-request-identity.sql"),
    ),
    (
        4,
        "pending object deletions",
        include_str!("../../../../schema/postgres/004-pending-object-deletions.sql"),
    ),
    (
        5,
        "CI run log totals",
        include_str!("../../../../schema/postgres/005-ci-run-log-totals.sql"),
    ),
    (
        6,
        "replicas",
        include_str!("../../../../schema/postgres/006-replicas.sql"),
    ),
    (
        7,
        "default branch length",
        include_str!("../../../../schema/postgres/007-default-branch-length.sql"),
    ),
    (
        8,
        "CI execution inputs",
        include_str!("../../../../schema/postgres/008-ci-execution-inputs.sql"),
    ),
    (
        9,
        "indexes and watch counts",
        include_str!("../../../../schema/postgres/009-indexes-and-watch-counts.sql"),
    ),
    (
        10,
        "pull request stacks",
        include_str!("../../../../schema/postgres/010-pull-request-stacks.sql"),
    ),
    (
        11,
        "branch protection",
        include_str!("../../../../schema/postgres/011-branch-protection.sql"),
    ),
    (
        12,
        "merge methods",
        include_str!("../../../../schema/postgres/012-merge-methods.sql"),
    ),
    (
        13,
        "pull request milestones",
        include_str!("../../../../schema/postgres/013-pull-request-milestones.sql"),
    ),
    (
        14,
        "pull request viewed files",
        include_str!("../../../../schema/postgres/014-pull-request-viewed-files.sql"),
    ),
    (
        15,
        "force push allowances",
        include_str!("../../../../schema/postgres/015-force-push-allowances.sql"),
    ),
    (
        16,
        "repository pushes",
        include_str!("../../../../schema/postgres/016-repository-pushes.sql"),
    ),
    (
        17,
        "ai crawler policy",
        include_str!("../../../../schema/postgres/017-ai-crawler-policy.sql"),
    ),
    (
        18,
        "workflow dispatch inputs",
        include_str!("../../../../schema/postgres/018-workflow-dispatch-inputs.sql"),
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
            bail!("database migration {version} history/checksum mismatch; no changes applied");
        }
    }
    Ok(())
}
