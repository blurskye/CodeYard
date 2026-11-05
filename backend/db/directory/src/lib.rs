//! The global directory owns a separate PostgreSQL database; cells never use its credentials.
use anyhow::{ensure, Result};
use codeyard_db_core::{pool::PgPool, DbPool};
use codeyard_support::cells::Prefix;
use sqlx::{Postgres, Transaction};
use std::{future::Future, time::Duration};

mod claims;
mod registry;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Directory request is invalid")]
    Invalid,
    #[error("Directory key is already claimed")]
    Conflict,
    #[error("Directory access denied")]
    Denied,
    #[error("Directory is unavailable")]
    Unavailable,
}

#[derive(Clone, Copy)]
pub struct Config {
    pub request_seconds: u64,
    pub claim_seconds: i64,
    pub stale_seconds: i64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            request_seconds: 5,
            claim_seconds: 300,
            stale_seconds: 30,
        }
    }
}

#[derive(Clone)]
pub struct Directory {
    pool: PgPool,
    config: Config,
}
impl Directory {
    pub async fn connect(url: &str, config: Config) -> Result<Self> {
        ensure!(
            (1..=60).contains(&config.request_seconds)
                && (30..=3600).contains(&config.claim_seconds)
                && (5..=300).contains(&config.stale_seconds),
            Refusal::Invalid
        );
        ensure!(
            url.starts_with("postgres://") || url.starts_with("postgresql://"),
            Refusal::Invalid
        );
        let pool = tokio::time::timeout(
            Duration::from_secs(config.request_seconds),
            codeyard_db_core::init_db(url),
        )
        .await
        .map_err(|_| Refusal::Unavailable)?
        .map_err(|_| Refusal::Unavailable)?;
        let DbPool::Postgres(pool) = pool else {
            anyhow::bail!(Refusal::Invalid);
        };
        Ok(Self { pool, config })
    }

    pub async fn migrate(&self) -> Result<()> {
        self.run(codeyard_db_core::postgres::migrations::run_baseline(
            &self.pool,
            "directory",
            include_str!("../../../../schema/directory/0001.schema.sql"),
        ))
        .await
    }

    pub async fn close(&self) {
        DbPool::Postgres(self.pool.clone()).close().await;
    }

    async fn run<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(Duration::from_secs(self.config.request_seconds), work).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.is::<Refusal>() => Err(error),
            Ok(Err(error)) if error.downcast_ref::<sqlx::Error>().is_some_and(|error| matches!(error, sqlx::Error::Database(error) if error.code().as_deref()==Some("23505"))) => Err(Refusal::Conflict.into()),
            _ => Err(Refusal::Unavailable.into()),
        }
    }

    async fn begin(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        let timeout = (self.config.request_seconds * 1000).to_string();
        sqlx::query(
            "SELECT set_config('statement_timeout',$1,true),set_config('lock_timeout',$1,true)",
        )
        .bind(timeout)
        .execute(&mut *tx)
        .await?;
        Ok(tx)
    }
}

/// The transport must authenticate this public key. Retain its live enrollment through commit.
async fn owner(
    tx: &mut Transaction<'_, Postgres>,
    authenticated_key: &str,
    exclusive: bool,
) -> Result<Prefix> {
    ensure!(
        codeyard_support::tokens::is_hex_token(authenticated_key, 64),
        Refusal::Denied
    );
    let key = authenticated_key.to_ascii_lowercase();
    let cell: Option<i32> =
        sqlx::query_scalar("SELECT cell_id FROM directory_node_keys WHERE public_key=$1")
            .bind(&key)
            .fetch_optional(&mut **tx)
            .await?;
    let cell = cell.ok_or(Refusal::Denied)?;
    // Operator registry changes retain the cell before its keys. Do the same here,
    // then recheck the key, so revocation cannot race the mutation or invert locks.
    let lock = if exclusive { "FOR UPDATE" } else { "FOR SHARE" };
    let state: Option<String> = sqlx::query_scalar(&format!(
        "SELECT state FROM directory_cells WHERE id=$1 {lock}"
    ))
    .bind(cell)
    .fetch_optional(&mut **tx)
    .await?;
    ensure!(
        state.is_some_and(|state| state != "disabled"),
        Refusal::Denied
    );
    let live: Option<i32> = sqlx::query_scalar(
        "SELECT cell_id FROM directory_node_keys WHERE public_key=$1 AND cell_id=$2 FOR SHARE",
    )
    .bind(key)
    .bind(cell)
    .fetch_optional(&mut **tx)
    .await?;
    ensure!(live == Some(cell), Refusal::Denied);
    Ok(Prefix::try_from(i64::from(cell)).map_err(|_| Refusal::Unavailable)?)
}
