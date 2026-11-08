//! The global directory owns a separate PostgreSQL database; cells never use its credentials.
use anyhow::{ensure, Result};
use codeyard_db_core::{pool::PgPool, DbPool};
use codeyard_types::cells::Prefix;
use sqlx::{Postgres, Transaction};
use std::{collections::BTreeMap, future::Future, time::Duration};

pub use presence::MAXIMUM_PRESENCE_REGIONS;

mod claims;
mod presence;
mod registry;
mod search;
mod shards;

pub use shards::{Moved, ShardRow};

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
    pub migration_seconds: u64,
    pub claim_seconds: i64,
    pub stale_seconds: i64,
    /// Total bytes of parsed search keys and values, also bounded by the wire validator.
    pub search_query_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            request_seconds: 5,
            migration_seconds: 180,
            claim_seconds: 300,
            stale_seconds: 30,
            search_query_bytes: 8192,
        }
    }
}

/// The registry database is also the cluster named `main`. Other clusters hold only the
/// logical shards the registry's map gives them.
pub const MAIN_CLUSTER: &str = "main";
/// Shard clusters a directory process can open.
pub const MAXIMUM_CLUSTERS: usize = 64;

#[derive(Clone)]
pub struct Directory {
    pool: PgPool,
    clusters: BTreeMap<String, PgPool>,
    config: Config,
}
impl Directory {
    pub async fn connect(url: &str, config: Config) -> Result<Self> {
        ensure!(
            (1..=60).contains(&config.request_seconds)
                && (30..=600).contains(&config.migration_seconds)
                && (30..=3600).contains(&config.claim_seconds)
                && (5..=300).contains(&config.stale_seconds)
                && (1..=8192).contains(&config.search_query_bytes),
            Refusal::Invalid
        );
        Ok(Self {
            pool: open(url, config).await?,
            clusters: BTreeMap::new(),
            config,
        })
    }

    /// Adds a shard cluster by the name the shard map uses. Call `migrate` afterwards.
    pub async fn add_cluster(&mut self, name: &str, url: &str) -> Result<()> {
        ensure!(
            valid_cluster(name)
                && name != MAIN_CLUSTER
                && !self.clusters.contains_key(name)
                && self.clusters.len() < MAXIMUM_CLUSTERS,
            Refusal::Invalid
        );
        let pool = open(url, self.config).await?;
        self.clusters.insert(name.to_owned(), pool);
        Ok(())
    }

    /// Every database gets the same baseline; only the registry fills the shard map.
    pub async fn migrate(&self) -> Result<()> {
        for pool in std::iter::once(&self.pool).chain(self.clusters.values()) {
            self.run_for(
                self.config.migration_seconds,
                codeyard_db_core::postgres::migrations::run_baseline(
                    pool,
                    "directory",
                    include_str!("../../../../schema/directory/0001.schema.sql"),
                ),
            )
            .await?;
        }
        self.run(async {
            sqlx::query("INSERT INTO directory_shards(shard,cluster) SELECT shard,$1 FROM generate_series(0,255) AS shard ON CONFLICT DO NOTHING")
                .bind(MAIN_CLUSTER)
                .execute(&self.pool)
                .await?;
            Ok(())
        })
        .await
    }

    pub async fn close(&self) {
        for pool in std::iter::once(&self.pool).chain(self.clusters.values()) {
            DbPool::Postgres(pool.clone()).close().await;
        }
    }

    async fn run<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        self.run_for(self.config.request_seconds, work).await
    }

    async fn run_for<T>(&self, seconds: u64, work: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(Duration::from_secs(seconds), work).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.is::<Refusal>() => Err(error),
            Ok(Err(error)) if error.downcast_ref::<sqlx::Error>().is_some_and(|error| matches!(error, sqlx::Error::Database(error) if error.code().as_deref()==Some("23505"))) => Err(Refusal::Conflict.into()),
            _ => Err(Refusal::Unavailable.into()),
        }
    }

    async fn begin(&self) -> Result<Transaction<'static, Postgres>> {
        self.begin_on(&self.pool).await
    }

    async fn begin_on(&self, pool: &PgPool) -> Result<Transaction<'static, Postgres>> {
        let mut tx = pool.begin().await?;
        let timeout = (self.config.request_seconds * 1000).to_string();
        sqlx::query(
            "SELECT set_config('statement_timeout',$1,true),set_config('lock_timeout',$1,true)",
        )
        .bind(timeout)
        .execute(&mut *tx)
        .await?;
        Ok(tx)
    }

    /// A cluster the map names but this process was not given is unavailable, not invalid.
    fn cluster(&self, name: &str) -> Result<&PgPool> {
        if name == MAIN_CLUSTER {
            return Ok(&self.pool);
        }
        Ok(self.clusters.get(name).ok_or(Refusal::Unavailable)?)
    }
}

async fn open(url: &str, config: Config) -> Result<PgPool> {
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
    Ok(pool)
}

pub fn valid_cluster(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Commits on success and rolls back at once on failure. A dropped transaction only queues
/// its rollback until the pool takes the connection back, so a refused request would keep
/// its row locks, and an immediate cleanup with SKIP LOCKED would pass over those rows.
async fn settle<T>(tx: Transaction<'_, Postgres>, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// The transport must authenticate this public key. Retain its live enrollment through commit.
async fn owner(
    tx: &mut Transaction<'_, Postgres>,
    authenticated_key: &str,
    exclusive: bool,
) -> Result<Prefix> {
    ensure!(
        codeyard_types::tokens::is_hex_token(authenticated_key, 64),
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

/// Retain read enrollment through the statement that returns private directory keys.
async fn read_owner(tx: &mut Transaction<'_, Postgres>, peer: &str) -> Result<()> {
    ensure!(
        codeyard_types::tokens::is_hex_token(peer, 64),
        Refusal::Denied
    );
    let router: Option<String> = sqlx::query_scalar(
        "SELECT public_key FROM directory_router_keys WHERE public_key=$1 FOR SHARE",
    )
    .bind(peer.to_ascii_lowercase())
    .fetch_optional(&mut **tx)
    .await?;
    if router.is_none() {
        owner(tx, peer, false).await?;
    }
    Ok(())
}
