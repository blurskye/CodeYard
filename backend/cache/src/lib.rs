//! Disposable content-derived values only. Callers must authorize before every lookup.
//! Never use this crate for sessions, credentials, permissions, rate limits or locks.
mod local;
mod remote;

use anyhow::{ensure, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

const BUDGET: Duration = Duration::from_millis(50);
const MAX_VALUE: usize = 1024 * 1024;
const MAX_BATCH: usize = 256;
const MAX_TTL: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Local,
    Standalone,
    Cluster,
}

#[derive(Clone, Copy, Debug)]
pub enum Family {
    Markdown,
    Diff,
}
impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Diff => "diff",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Markdown => 0,
            Self::Diff => 1,
        }
    }
}

pub struct Config {
    pub mode: Mode,
    pub urls: Vec<String>,
    pub deployment: String,
    pub cell: u16,
    pub local_entries: usize,
    pub local_bytes: usize,
}
impl Config {
    pub fn local(deployment: &str, cell: u16) -> Self {
        Self {
            mode: Mode::Local,
            urls: vec![],
            deployment: deployment.into(),
            cell,
            local_entries: 2048,
            local_bytes: 32 * 1024 * 1024,
        }
    }
    pub fn from_env(cell: u16) -> Result<Self> {
        let mut config = Self::local(
            &std::env::var("CACHE_DEPLOYMENT").unwrap_or_else(|_| "default".into()),
            cell,
        );
        config.mode = match std::env::var("CACHE_MODE").as_deref() {
            Ok("local") | Err(std::env::VarError::NotPresent) => Mode::Local,
            Ok("standalone") => Mode::Standalone,
            Ok("cluster") => Mode::Cluster,
            _ => anyhow::bail!("Invalid CACHE_MODE"),
        };
        config.urls = std::env::var("CACHE_URLS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .collect();
        if let Ok(value) = std::env::var("CACHE_LOCAL_ENTRIES") {
            config.local_entries = value.parse()?;
        }
        if let Ok(value) = std::env::var("CACHE_LOCAL_BYTES") {
            config.local_bytes = value.parse()?;
        }
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.deployment.is_empty()
                && self.deployment.len() <= 64
                && self
                    .deployment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid cache deployment"
        );
        ensure!(
            (1..=65536).contains(&self.local_entries)
                && (1024..=1024 * 1024 * 1024).contains(&self.local_bytes),
            "Invalid local cache bounds"
        );
        ensure!(
            match self.mode {
                Mode::Local => true,
                Mode::Standalone => self.urls.len() == 1,
                Mode::Cluster => !self.urls.is_empty() && self.urls.len() <= 16,
            },
            "Invalid CACHE_URLS count"
        );
        ensure!(self.urls.len() <= 16, "Too many cache endpoints");
        for url in &self.urls {
            ensure!(
                url.len() <= 4096
                    && (url.starts_with("redis://") || url.starts_with("rediss://"))
                    && !url.contains('#'),
                "Invalid cache endpoint"
            );
            let client = redis::Client::open(url.as_str())
                .map_err(|_| anyhow::anyhow!("Invalid cache endpoint"))?;
            ensure!(
                client.get_connection_info().redis.db == 0,
                "Cache endpoints must use database zero"
            );
        }
        Ok(())
    }
}

/// Constructed only from a namespace, a fixed family and length-framed content/options.
#[derive(Clone, Debug)]
pub struct Key {
    text: String,
    family: Family,
}
impl Key {
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

pub struct Cache {
    namespace: String,
    local: Mutex<local::Local>,
    remote: Option<remote::Remote>,
    flights: [AsyncMutex<()>; 64],
    recomputes: [Semaphore; 2],
}
impl Cache {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            namespace: format!("cy:v1:{}:{}:", config.deployment, config.cell),
            local: Mutex::new(local::Local::new(config.local_entries, config.local_bytes)),
            remote: remote::Remote::new(config.mode, &config.urls)?,
            flights: std::array::from_fn(|_| AsyncMutex::new(())),
            recomputes: std::array::from_fn(|_| Semaphore::new(16)),
        })
    }
    pub fn key(&self, family: Family, parts: &[&[u8]]) -> Key {
        let mut digest = Sha256::new();
        for part in parts {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part);
        }
        Key {
            text: format!(
                "{}{}:{:x}",
                self.namespace,
                family.name(),
                digest.finalize()
            ),
            family,
        }
    }
    fn belongs(&self, key: &Key) -> bool {
        key.text.starts_with(&self.namespace)
    }
    fn local_get(&self, key: &Key) -> Option<Arc<[u8]>> {
        if !self.belongs(key) {
            return None;
        }
        self.local
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key.text)
    }
    fn accept(&self, key: &Key, bytes: Vec<u8>) -> Option<Arc<[u8]>> {
        if bytes.len() < 8 || bytes.len() > MAX_VALUE + 8 {
            return None;
        }
        let expiry = u64::from_be_bytes(bytes[..8].try_into().ok()?);
        let remaining = expiry.checked_sub(now_ms()?)?;
        if remaining == 0 || remaining > MAX_TTL.as_millis() as u64 {
            return None;
        }
        let value: Arc<[u8]> = Arc::from(&bytes[8..]);
        self.local.lock().unwrap_or_else(|e| e.into_inner()).put(
            key.text.clone(),
            value.clone(),
            Duration::from_millis(remaining),
        );
        Some(value)
    }
    pub async fn get(&self, key: &Key) -> Option<Arc<[u8]>> {
        if !self.belongs(key) {
            return None;
        }
        if let Some(value) = self.local_get(key) {
            return Some(value);
        }
        let remote = self.remote.as_ref()?;
        let value: Option<Vec<u8>> = remote.query(redis::cmd("GET").arg(&key.text)).await?;
        self.accept(key, value?)
    }
    /// At most 256 keys per call. Failed slots are misses, without reordering other slots.
    pub async fn mget(&self, keys: &[Key]) -> Result<Vec<Option<Arc<[u8]>>>> {
        ensure!(keys.len() <= MAX_BATCH, "Cache batch exceeds its limit");
        let mut result: Vec<_> = keys.iter().map(|key| self.local_get(key)).collect();
        let Some(remote) = &self.remote else {
            return Ok(result);
        };
        let deadline = tokio::time::Instant::now() + BUDGET;
        let mut groups: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
        for (index, key) in keys.iter().enumerate() {
            if result[index].is_none() && self.belongs(key) {
                groups
                    .entry(remote::slot(&key.text))
                    .or_default()
                    .push(index);
            }
        }
        let mut pending = FuturesUnordered::new();
        for indexes in groups.into_values() {
            pending.push(async move {
                let mut command = redis::cmd("MGET");
                for index in &indexes {
                    command.arg(&keys[*index].text);
                }
                let values: Option<Vec<Option<Vec<u8>>>> = remote.query(&command).await;
                (indexes, values)
            });
        }
        while let Ok(Some((indexes, values))) =
            tokio::time::timeout_at(deadline, pending.next()).await
        {
            if let Some(values) = values.filter(|values| values.len() == indexes.len()) {
                for (index, value) in indexes.into_iter().zip(values) {
                    result[index] = value.and_then(|value| self.accept(&keys[index], value));
                }
            }
        }
        if !pending.is_empty() {
            remote.failed();
        }
        Ok(result)
    }
    /// Every local and remote insertion has a TTL. Oversized values bypass both tiers.
    pub async fn put(&self, key: &Key, value: Arc<[u8]>, ttl: Duration) {
        if !self.belongs(key) || value.len() > MAX_VALUE || ttl.is_zero() || ttl > MAX_TTL {
            return;
        }
        let millis = ttl.as_millis() as u64;
        if millis == 0 {
            return;
        }
        self.local.lock().unwrap_or_else(|e| e.into_inner()).put(
            key.text.clone(),
            value.clone(),
            ttl,
        );
        let Some(remote) = &self.remote else {
            return;
        };
        let Some(expiry) = now_ms().and_then(|now| now.checked_add(millis)) else {
            return;
        };
        let mut payload = expiry.to_be_bytes().to_vec();
        payload.extend_from_slice(&value);
        let _: Option<()> = remote
            .query(
                redis::cmd("SET")
                    .arg(&key.text)
                    .arg(payload)
                    .arg("PX")
                    .arg(millis),
            )
            .await;
    }
    pub async fn delete(&self, keys: &[Key]) -> Result<()> {
        ensure!(keys.len() <= MAX_BATCH, "Cache batch exceeds its limit");
        let mut groups: BTreeMap<u16, Vec<&str>> = BTreeMap::new();
        {
            let mut local = self.local.lock().unwrap_or_else(|e| e.into_inner());
            for key in keys.iter().filter(|key| self.belongs(key)) {
                local.remove(&key.text);
                groups
                    .entry(remote::slot(&key.text))
                    .or_default()
                    .push(&key.text);
            }
        }
        let Some(remote) = &self.remote else {
            return Ok(());
        };
        let deadline = tokio::time::Instant::now() + BUDGET;
        let mut pending = FuturesUnordered::new();
        for keys in groups.into_values() {
            pending.push(async move {
                let _: Option<u64> = remote.query(redis::cmd("DEL").arg(keys)).await;
            });
        }
        while let Ok(Some(())) = tokio::time::timeout_at(deadline, pending.next()).await {}
        if !pending.is_empty() {
            remote.failed();
        }
        Ok(())
    }
    /// The recompute limit is always on, including local mode. Cache failure never bypasses
    /// it. Striped single-flight locks are bounded; cancellation releases both permits.
    pub async fn get_or_try_init<E, F, Fut>(
        &self,
        key: &Key,
        ttl: Duration,
        compute: F,
    ) -> std::result::Result<Arc<[u8]>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = std::result::Result<Vec<u8>, E>>,
    {
        let _flight = self.flights[usize::from(remote::slot(&key.text)) % self.flights.len()]
            .lock()
            .await;
        if let Some(value) = self.get(key).await {
            return Ok(value);
        }
        let _permit = self.recomputes[key.family.index()]
            .acquire()
            .await
            .expect("cache semaphore is never closed");
        let value: Arc<[u8]> = Arc::from(compute().await?);
        self.put(key, value.clone(), ttl).await;
        Ok(value)
    }
}
fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_mode_preserves_order_namespace_ttl_and_deletion() {
        let cache = Cache::new(Config::local("test", 3)).unwrap();
        let a = cache.key(Family::Markdown, &[b"ab", b"c"]);
        let b = cache.key(Family::Markdown, &[b"a", b"bc"]);
        assert_ne!(a.as_str(), b.as_str());
        assert!(a.as_str().starts_with("cy:v1:test:3:markdown:"));
        assert!(!a.as_str().contains(['{', '}']));
        cache
            .put(&a, Arc::from(&b"value"[..]), Duration::from_secs(1))
            .await;
        let result = cache
            .mget(&[b.clone(), a.clone(), a.clone()])
            .await
            .unwrap();
        assert!(result[0].is_none());
        assert_eq!(result[1].as_deref(), Some(&b"value"[..]));
        assert_eq!(result[1], result[2]);
        let other = Cache::new(Config::local("other", 3)).unwrap();
        assert!(other.get(&a).await.is_none());
        cache.delete(&[a.clone(), b]).await.unwrap();
        assert!(cache.get(&a).await.is_none());
        cache
            .put(&a, Arc::from(&b"value"[..]), Duration::ZERO)
            .await;
        assert!(cache.get(&a).await.is_none());
    }
    #[tokio::test]
    async fn cancellation_releases_single_flight_and_recompute_capacity() {
        let cache = Arc::new(Cache::new(Config::local("test", 0)).unwrap());
        let started = Arc::new(tokio::sync::Notify::new());
        let key = cache.key(Family::Markdown, &[b"cancelled"]);
        let worker_cache = cache.clone();
        let worker_key = key.clone();
        let worker_started = started.clone();
        let job = tokio::spawn(async move {
            worker_cache
                .get_or_try_init(&worker_key, Duration::from_secs(1), || async {
                    worker_started.notify_one();
                    std::future::pending::<std::result::Result<Vec<u8>, ()>>().await
                })
                .await
        });
        started.notified().await;
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        let value = tokio::time::timeout(
            Duration::from_secs(1),
            cache.get_or_try_init(&key, Duration::from_secs(1), || async {
                Ok::<_, ()>(b"retry".to_vec())
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&*value, b"retry");
    }

    #[tokio::test]
    async fn concurrent_misses_compute_once_and_failed_computes_retry() {
        let cache = Cache::new(Config::local("test", 0)).unwrap();
        let key = cache.key(Family::Diff, &[b"immutable"]);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let load = || {
            cache.get_or_try_init(&key, Duration::from_secs(1), || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok::<_, ()>(b"rendered".to_vec())
            })
        };
        let (a, b, c) = tokio::join!(load(), load(), load());
        assert_eq!(a.unwrap(), b.unwrap());
        assert!(c.is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        cache.delete(std::slice::from_ref(&key)).await.unwrap();
        assert!(cache
            .get_or_try_init(&key, Duration::from_secs(1), || async {
                Err::<Vec<u8>, _>(())
            })
            .await
            .is_err());
        assert!(load().await.is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
