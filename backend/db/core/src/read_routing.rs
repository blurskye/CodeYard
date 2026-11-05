//! Optional PostgreSQL content reads; authority and transactions always use the primary.
use crate::{pool::PgPool, DbPool};
use anyhow::{ensure, Result};
use std::{future::Future, time::Duration};

pub enum Credential<'a> {
    Session(&'a str),
    Token(&'a str),
}
struct ReadScope {
    primary: PgPool,
    replica: PgPool,
}
tokio::task_local! { static CONTENT: Option<ReadScope>; }

/// An unavailable replica never prevents startup. Each request proves identity and replay
/// before selecting it, and a promoted replica is not treated as a read-only target.
pub async fn configure(pool: &DbPool) -> Result<()> {
    let url = match std::env::var("DATABASE_READ_URL") {
        Ok(url) if !url.is_empty() => url,
        Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(()),
        Err(_) => anyhow::bail!("invalid DATABASE_READ_URL"),
    };
    let DbPool::Postgres(primary) = pool else {
        anyhow::bail!("DATABASE_READ_URL requires PostgreSQL");
    };
    ensure!(
        url.starts_with("postgres://") || url.starts_with("postgresql://"),
        "DATABASE_READ_URL requires PostgreSQL"
    );
    let options = crate::connection::postgres_options(&url)?
        .options([("default_transaction_read_only", "on")]);
    let size = std::env::var("DATABASE_POOL_SIZE").ok();
    let replica = sqlx::postgres::PgPoolOptions::new()
        .max_connections(crate::connection::postgres_pool_size(size.as_deref())?)
        .test_before_acquire(true)
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy_with(options)
        .into();
    *primary
        .read_replica
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(replica);
    Ok(())
}

pub fn has_replica(pool: &DbPool) -> bool {
    match pool {
        DbPool::Postgres(pool) => pool
            .read_replica
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .is_some(),
        DbPool::Sqlite(_) => false,
    }
}

async fn current_replica(primary: &PgPool) -> Option<ReadScope> {
    let replica = primary
        .read_replica
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()?;
    // Fail back to primary within two seconds. A behind replica is skipped immediately,
    // without waiting for recovery while retaining an application connection.
    tokio::time::timeout(Duration::from_secs(2), async {
        let (lsn, identity): (String, String) = sqlx::query_as(
            "SELECT pg_current_wal_lsn()::text,database_id FROM node_configuration WHERE id=1"
        ).fetch_one(primary).await?;
        let ready: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM node_configuration WHERE id=1 AND database_id=$2 AND pg_is_in_recovery() AND pg_last_wal_replay_lsn()>=$1::pg_lsn)"
        ).bind(lsn).bind(identity).fetch_one(&replica).await?;
        Ok::<_, sqlx::Error>(ready.then(|| ReadScope { primary: primary.clone(), replica }))
    }).await.ok()?.ok()?
}

pub async fn scope<T>(pool: &DbPool, eligible: bool, work: impl Future<Output = T>) -> T {
    let selected = match (pool, eligible) {
        (DbPool::Postgres(primary), true) => current_replica(primary).await,
        _ => None,
    };
    CONTENT.scope(selected, work).await
}

pub fn content_pool(pool: &DbPool) -> DbPool {
    CONTENT
        .try_with(|scope| match (pool, scope) {
            (DbPool::Postgres(primary), Some(scope)) if primary.same_pool(&scope.primary) => {
                DbPool::Postgres(scope.replica.clone())
            }
            _ => pool.clone(),
        })
        .unwrap_or_else(|_| pool.clone())
}

/// Routing hints can only pin a live, existing credential to the primary. They grant no
/// access and never create rows for attacker-supplied credentials.
pub async fn primary_required(
    pool: &DbPool,
    credential: Credential<'_>,
    write: bool,
) -> Result<bool> {
    let valid = match &credential {
        Credential::Session(value) => codeyard_support::cells::Secret::cookie(value).is_ok(),
        Credential::Token(value) => codeyard_support::tokens::is_hex_token(value, 64),
    };
    let (table, key, value) = match credential {
        Credential::Session(value) => ("sessions", "id", value),
        Credential::Token(value) => ("personal_access_tokens", "token_hash", value),
    };
    ensure!(valid, "invalid routing credential");
    let read = |clock: &str| {
        format!(
            "SELECT EXISTS(SELECT 1 FROM {table} WHERE {key}=$1 AND read_primary_until>{clock})"
        )
    };
    let update =
        |clock: &str| format!("UPDATE {table} SET read_primary_until={clock}+5 WHERE {key}=$1");
    match pool {
        DbPool::Postgres(pool) => {
            let clock = "FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint";
            if write {
                sqlx::query(&update(clock))
                    .bind(value)
                    .execute(pool)
                    .await?;
                return Ok(true);
            }
            Ok(sqlx::query_scalar(&read(clock))
                .bind(value)
                .fetch_one(pool)
                .await?)
        }
        DbPool::Sqlite(pool) => {
            let clock = "CAST(strftime('%s','now') AS INTEGER)";
            if write {
                sqlx::query(&update(clock))
                    .bind(value)
                    .execute(pool)
                    .await?;
                return Ok(true);
            }
            Ok(sqlx::query_scalar(&read(clock))
                .bind(value)
                .fetch_one(pool)
                .await?)
        }
    }
}
