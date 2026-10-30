//! PostgreSQL schema migrations.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::info;

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "schema", include_str!("../../../../schema/postgres/0001.schema.sql")),
    (2, "admission", include_str!("../../../../schema/postgres/0002.admission.sql")),
    (3, "registry_compatibility", include_str!("../../../../schema/postgres/0003.registry_compatibility.sql")),
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
        if version == 3 {
            let mut after = 0i64;
            loop {
                let rows: Vec<(i64, String, String)> = sqlx::query_as("SELECT v.id,v.name,v.manifest FROM package_versions v JOIN packages p ON p.id=v.package_id WHERE p.package_type='nuget' AND v.id>$1 ORDER BY v.id LIMIT 16")
                    .bind(after).fetch_all(&mut *tx).await?;
                if rows.is_empty() { break; }
                for (id, name, manifest) in rows {
                    let mut metadata: serde_json::Value = serde_json::from_str(&manifest)?;
                    metadata["semver2"] = crate::backend::nuget::metadata_semver2(&name, &metadata)?.into();
                    sqlx::query("UPDATE package_versions SET manifest=$1 WHERE id=$2")
                        .bind(metadata.to_string()).bind(id).execute(&mut *tx).await?;
                    after = id;
                }
            }
        }
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
