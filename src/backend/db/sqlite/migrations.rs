//! SQLite schema migrations.
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "baseline",
        include_str!("../../../../schema/sqlite/schema.sql"),
    ),
    (
        2,
        "CI context names",
        include_str!("../../../../schema/sqlite/002-ci-contexts.sql"),
    ),
    (
        3,
        "CI pull request identity",
        include_str!("../../../../schema/sqlite/003-ci-pull-request-identity.sql"),
    ),
    (
        4,
        "pending object deletions",
        include_str!("../../../../schema/sqlite/004-pending-object-deletions.sql"),
    ),
    (
        5,
        "CI run log totals",
        include_str!("../../../../schema/sqlite/005-ci-run-log-totals.sql"),
    ),
    (
        6,
        "replicas",
        include_str!("../../../../schema/sqlite/006-replicas.sql"),
    ),
    (
        7,
        "default branch length",
        include_str!("../../../../schema/sqlite/007-default-branch-length.sql"),
    ),
    (
        8,
        "CI execution inputs",
        include_str!("../../../../schema/sqlite/008-ci-execution-inputs.sql"),
    ),
    (
        9,
        "indexes and watch counts",
        include_str!("../../../../schema/sqlite/009-indexes-and-watch-counts.sql"),
    ),
    (
        10,
        "pull request stacks",
        include_str!("../../../../schema/sqlite/010-pull-request-stacks.sql"),
    ),
    (
        11,
        "branch protection",
        include_str!("../../../../schema/sqlite/011-branch-protection.sql"),
    ),
    (
        12,
        "merge methods",
        include_str!("../../../../schema/sqlite/012-merge-methods.sql"),
    ),
    (
        13,
        "pull request milestones",
        include_str!("../../../../schema/sqlite/013-pull-request-milestones.sql"),
    ),
    (
        14,
        "pull request viewed files",
        include_str!("../../../../schema/sqlite/014-pull-request-viewed-files.sql"),
    ),
    (
        15,
        "force push allowances",
        include_str!("../../../../schema/sqlite/015-force-push-allowances.sql"),
    ),
    (
        16,
        "repository pushes",
        include_str!("../../../../schema/sqlite/016-repository-pushes.sql"),
    ),
    (
        17,
        "ai crawler policy",
        include_str!("../../../../schema/sqlite/017-ai-crawler-policy.sql"),
    ),
    (
        18,
        "workflow dispatch inputs",
        include_str!("../../../../schema/sqlite/018-workflow-dispatch-inputs.sql"),
    ),
];

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
            bail!("SQLite migration history/checksum mismatch; no changes applied");
        }
    }
    Ok(())
}
