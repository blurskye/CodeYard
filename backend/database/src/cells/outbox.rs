//! Cleanup survives a native commit, a lost reply and process shutdown. Delivery is idempotent.
use super::directory::{self, Refusal};
use crate::DbPool;
use anyhow::{ensure, Result};
use codeyard_types::cells::{Key, KeyKind, Object};
use serde::{Deserialize, Serialize};

const TOPIC: &str = "directory.cleanup";
const POINTER_TOPIC: &str = "pointer";
const PUSH_TOPIC: &str = "repository.push";
const REFRESH_TOPIC: &str = "pull.refresh";

pub trait Consumer: Send + Sync {
    /// One pull per delivery keeps the cursor durable and the queue fair.
    fn refresh_pull_head<'a>(
        &'a self,
        refresh: &'a mut super::pulls::Refresh,
    ) -> futures_util::future::BoxFuture<'a, Result<bool>>;
    /// Writes one repository's, issue's or pull request's current documents to the index.
    fn index_search<'a>(
        &'a self,
        index: &'a dyn super::search::Index,
        repository: i64,
        item: super::search::Item,
        version: super::search::Version,
    ) -> futures_util::future::BoxFuture<'a, Result<()>>;
}
const MAXIMUM_BYTES: usize = 524288;
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Delivery {
    Release { object: Object, keys: Vec<Key> },
    Forget { object: Object },
}
impl Delivery {
    fn object(&self) -> Object {
        match self {
            Self::Release { object, .. } | Self::Forget { object } => *object,
        }
    }
    fn validate(&self) -> Result<()> {
        let object = self.object();
        object.validate()?;
        if let Self::Release { keys, .. } = self {
            ensure!(
                keys.len() <= 1000
                    && keys
                        .iter()
                        .all(|key| key.kind() != KeyKind::Name && key.belongs_to(object.kind)),
                Refusal::Denied
            );
        }
        Ok(())
    }
}
macro_rules! engine {
    ($module:ident, $pool:ty, $connection:ty) => {
        pub mod $module {
            use super::*;
            async fn queue(
                pool: &$pool,
                connection: &mut $connection,
                delivery: Delivery,
            ) -> Result<()> {
                if pool.directory().is_none() {
                    return Ok(());
                }
                delivery.validate()?;
                let object = delivery.object();
                let payload = serde_json::to_string(&delivery)?;
                ensure!(payload.len() <= MAXIMUM_BYTES, Refusal::Denied);
                sqlx::query(
                    "INSERT INTO cell_outbox(root_kind,root_id,topic,payload) VALUES($1,$2,$3,$4)",
                )
                .bind(object.kind.as_str())
                .bind(object.id)
                .bind(TOPIC)
                .bind(payload)
                .execute(connection)
                .await?;
                Ok(())
            }
            pub async fn release(
                pool: &$pool,
                connection: &mut $connection,
                object: Object,
                keys: &[Key],
            ) -> Result<()> {
                if pool.directory().is_none() {
                    return Ok(());
                }
                ensure!(keys.len() <= 1000, Refusal::Denied);
                queue(
                    pool,
                    connection,
                    Delivery::Release {
                        object,
                        keys: keys.to_vec(),
                    },
                )
                .await
            }
            pub async fn forget(
                pool: &$pool,
                connection: &mut $connection,
                object: Object,
            ) -> Result<()> {
                queue(pool, connection, Delivery::Forget { object }).await
            }
        }
    };
}
engine!(postgres, crate::pool::PgPool, sqlx::PgConnection);
engine!(sqlite, crate::pool::SqlitePool, sqlx::SqliteConnection);

/// A claim leases one event; the lease outlives the bounded delivery, so no row lock or
/// transaction is held across the network call. A crash or timeout leaves the event to be
/// retried after the lease; only the latest claim records a result. Repeating an effect is safe.
pub async fn deliver_one(pool: &DbPool) -> Result<bool> {
    deliver(pool, None).await
}
pub async fn deliver_with(pool: &DbPool, consumer: &dyn Consumer) -> Result<bool> {
    deliver(pool, Some(consumer)).await
}
async fn deliver(pool: &DbPool, consumer: Option<&dyn Consumer>) -> Result<bool> {
    let DbPool::Postgres(native) = pool else {
        return Ok(false);
    };
    let Some(provider) = native.directory() else {
        return Ok(false);
    };
    let config = provider.config();
    config.validate()?;
    let request = config.request_seconds.max(1);
    let deadline = (config.request_seconds * 30).clamp(1, 300);
    // Claim and record get their own request time on top of the delivery deadline.
    let whole = deadline + 2 * request + 2;
    let lease = (whole + request) as i64;
    tokio::time::timeout(std::time::Duration::from_secs(whole), async {
        let row: Option<(i64, String, String, i64, String, i64)> = sqlx::query_as("UPDATE cell_outbox SET attempts=attempts+1,next_attempt_at=clock_timestamp()+$7::bigint*INTERVAL '1 second' WHERE id=(SELECT id FROM cell_outbox WHERE topic IN ($1,$2,$3,$4,$6,$8) AND (topic NOT IN ($4,$8) OR $5) AND delivered_at IS NULL AND next_attempt_at<=clock_timestamp() ORDER BY next_attempt_at,id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id,substr(payload,1,524289),root_kind,root_id,topic,attempts")
            .bind(TOPIC).bind(POINTER_TOPIC).bind(PUSH_TOPIC).bind(REFRESH_TOPIC).bind(consumer.is_some()).bind(super::notices::TOPIC).bind(lease).bind(super::search::TOPIC)
            .fetch_optional(native).await?;
        let Some((id, payload, kind, root, topic, attempt)) = row else { return Ok(false); };
        let mut continuation = None;
        let operation = async {
            ensure!(payload.len() <= MAXIMUM_BYTES, Refusal::Unavailable);
            if topic == PUSH_TOPIC {
                let change: super::pulls::Push = serde_json::from_str(&payload)?;
                change.validate()?;
                ensure!(kind == "repository" && root == change.head_repo_id, Refusal::Denied);
                let provider = native.foreign_authorization().ok_or(Refusal::Unavailable)?;
                return provider.send_push(&super::pulls::Event { id, change }).await;
            }
            if topic == REFRESH_TOPIC {
                let mut refresh: super::pulls::Refresh = serde_json::from_str(&payload)?;
                refresh.validate()?;
                ensure!(kind == "repository" && root == refresh.push.base_repo_id, Refusal::Denied);
                let complete = consumer.ok_or(Refusal::Unavailable)?.refresh_pull_head(&mut refresh).await?;
                refresh.validate()?;
                if !complete { continuation = Some(serde_json::to_string(&refresh)?); }
                return Ok(());
            }
            if topic == super::search::TOPIC {
                let item: super::search::Item = serde_json::from_str(&payload)?;
                item.validate()?;
                ensure!(kind == "repository", Refusal::Denied);
                let index = native.search().ok_or(Refusal::Unavailable)?;
                let version = super::search::Version { event: id, attempt };
                return consumer.ok_or(Refusal::Unavailable)?.index_search(&*index, root, item, version).await;
            }
            if topic == super::notices::TOPIC {
                let notice: super::notices::Notice = serde_json::from_str(&payload)?;
                notice.validate(native.cell_id())?;
                ensure!(kind == "repository" && root == notice.repository.id, Refusal::Denied);
                // Replayed notices refresh recipients after privacy, grant or deletion changes.
                let public: Option<bool> = sqlx::query_scalar("SELECT NOT is_private AND NOT is_internal FROM repositories WHERE id=$1 AND NOT is_stub")
                    .bind(root).fetch_optional(native).await?;
                let home = native.foreign_authorization().ok_or(Refusal::Unavailable)?;
                return home.send_notice(&super::notices::Event { id, notice, withdrawn: public != Some(true) }).await;
            }
            if topic == POINTER_TOPIC {
                let change: super::pointers::Change = serde_json::from_str(&payload)?;
                change.validate()?;
                ensure!(change.object.kind.as_str() == kind && change.object.id == root, Refusal::Denied);
                let home = native.foreign_authorization().ok_or(Refusal::Unavailable)?;
                return home.send_pointer(&super::pointers::Event {id,change}).await;
            }
            let delivery: Delivery = serde_json::from_str(&payload)?;
            delivery.validate()?;
            ensure!(delivery.object().kind.as_str() == kind && delivery.object().id == root, Refusal::Denied);
            match delivery {
                Delivery::Release { object, keys } => directory::postgres::release(native, object, &keys).await,
                Delivery::Forget { object } => directory::postgres::forget(native, object).await,
            }
        };
        let limit = if topic == REFRESH_TOPIC || topic == super::search::TOPIC { deadline } else { config.request_seconds * 2 + 1 };
        let result: Result<()> = tokio::time::timeout(std::time::Duration::from_secs(limit), operation)
            .await.unwrap_or_else(|_| Err(Refusal::Unavailable.into()));
        // A claim whose lease ran out and was taken again records nothing.
        let recorded = match result {
            Ok(()) => match continuation {
                Some(payload) => {
                    ensure!(payload.len() <= MAXIMUM_BYTES, Refusal::Unavailable);
                    sqlx::query("UPDATE cell_outbox SET payload=$3,next_attempt_at=clock_timestamp() WHERE id=$1 AND attempts=$2 AND delivered_at IS NULL")
                        .bind(id).bind(attempt).bind(payload).execute(native).await?
                }
                None => sqlx::query("UPDATE cell_outbox SET delivered_at=clock_timestamp() WHERE id=$1 AND attempts=$2 AND delivered_at IS NULL")
                    .bind(id).bind(attempt).execute(native).await?,
            },
            Err(error) => {
                tracing::warn!(event_id=id, %topic, error=%format!("{error:#}"), "Cell event remains queued");
                sqlx::query("UPDATE cell_outbox SET next_attempt_at=clock_timestamp()+$3::bigint*INTERVAL '1 second' WHERE id=$1 AND attempts=$2 AND delivered_at IS NULL")
                    .bind(id).bind(attempt).bind(config.request_seconds as i64).execute(native).await?
            }
        };
        // A search document changed again while in flight; its newer claim sends it again.
        if recorded.rows_affected() == 0 && topic != super::search::TOPIC {
            tracing::warn!(event_id=id, %topic, "Cell event was claimed again before its result was recorded");
        }
        Ok(true)
    }).await.map_err(|_| Refusal::Unavailable)?
}

pub async fn run(
    pool: DbPool,
    consumer: std::sync::Arc<dyn Consumer>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if *stop.borrow() {
            return;
        }
        tokio::select! { biased;
            _ = stop.changed() => return,
            _ = interval.tick() => {}
        }
        // Work per tick is bounded even while a large backlog is ready.
        for _ in 0..64 {
            let result = tokio::select! { biased;
                _ = stop.changed() => return,
                result = deliver_with(&pool, &*consumer) => result,
            };
            match result {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => {
                    tracing::warn!("Cell outbox is unavailable");
                    break;
                }
            }
        }
    }
}
