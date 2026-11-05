//! Bounded, short-lived directory routes. No database credentials or user secrets are cached.
use crate::directory::{Client, Failure, Reply, Request};
use anyhow::{ensure, Result};
use codeyard_model::cells::{Cell, Key, KeyKind, Location, Object, State};
use codeyard_support::{cells::Prefix, token_hash::token_hash};
use std::{
    collections::HashMap,
    mem::size_of,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub struct Config {
    pub cache_seconds: u64,
    pub maximum_entries: usize,
    pub maximum_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            cache_seconds: 60,
            maximum_entries: 10000,
            maximum_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum RouteKey {
    Key([u8; 32]),
    Home(&'static str, i64),
    Cell(Prefix),
}
#[derive(Clone)]
enum Value {
    Location(Location),
    Home(Prefix),
    Cell(Arc<Cell>),
}
struct Entry {
    value: Value,
    until: Instant,
    bytes: usize,
}
struct Cache {
    config: Config,
    entries: HashMap<RouteKey, Entry>,
    bytes: usize,
}
impl Cache {
    fn new(config: Config) -> Result<Self> {
        ensure!(
            (1..=60).contains(&config.cache_seconds)
                && (1..=100000).contains(&config.maximum_entries)
                && (1024..=1024 * 1024 * 1024).contains(&config.maximum_bytes),
            "Invalid route cache limits"
        );
        Ok(Self {
            config,
            entries: HashMap::new(),
            bytes: 0,
        })
    }
    fn get(&self, key: RouteKey, now: Instant) -> Option<Value> {
        self.entries
            .get(&key)
            .filter(|entry| entry.until > now)
            .map(|entry| entry.value.clone())
    }
    fn put(&mut self, key: RouteKey, value: Value, now: Instant) {
        let mut bytes = size_of::<(RouteKey, Entry)>() + 64;
        if let Value::Cell(cell) = &value {
            bytes += size_of::<Cell>() + cell.name.capacity() + cell.region.capacity();
            for strings in [&cell.web_urls, &cell.ssh_addresses, &cell.node_keys] {
                bytes += strings.capacity() * size_of::<String>();
                bytes += strings.iter().map(String::capacity).sum::<usize>();
            }
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.bytes;
        }
        if bytes > self.config.maximum_bytes {
            return;
        }
        // Clearing a full cache is bounded and avoids an unbounded eviction walk.
        // A cache miss repeats a lookup; it never changes the owning cell.
        if self.entries.len() >= self.config.maximum_entries
            || self.bytes + bytes > self.config.maximum_bytes
        {
            self.entries = HashMap::new();
            self.bytes = 0;
        }
        self.entries.insert(
            key,
            Entry {
                value,
                until: now + Duration::from_secs(self.config.cache_seconds),
                bytes,
            },
        );
        self.bytes += bytes;
    }
}

#[derive(Clone)]
pub struct Routes {
    directory: Client,
    cache: Arc<RwLock<Cache>>,
}
impl Routes {
    pub fn new(directory: Client, config: Config) -> Result<Self> {
        Ok(Self {
            directory,
            cache: Arc::new(RwLock::new(Cache::new(config)?)),
        })
    }
    fn get(&self, key: RouteKey) -> Option<Value> {
        self.cache.read().ok()?.get(key, Instant::now())
    }
    fn put(&self, key: RouteKey, value: Value) {
        if let Ok(mut cache) = self.cache.write() {
            cache.put(key, value, Instant::now());
        }
    }
    pub fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.entries = HashMap::new();
            cache.bytes = 0;
        }
    }
    pub async fn cell(&self, prefix: Prefix) -> Result<Option<Arc<Cell>>> {
        if let Some(Value::Cell(cell)) = self.get(RouteKey::Cell(prefix)) {
            return Ok(Some(cell));
        }
        let Reply::Cell(cell) = self.directory.request(&Request::Cell { prefix }).await? else {
            anyhow::bail!(Failure::Unavailable);
        };
        let Some(cell) = cell else {
            return Ok(None);
        };
        let cell = Arc::new(cell.prepare().map_err(|_| Failure::Unavailable)?);
        ensure!(cell.state != State::Disabled, Failure::Unavailable);
        self.put(RouteKey::Cell(prefix), Value::Cell(cell.clone()));
        Ok(Some(cell))
    }
    pub async fn location(&self, key: &Key) -> Result<Option<Location>> {
        let hash = RouteKey::Key(key.hash());
        if let Some(Value::Location(location)) = self.get(hash) {
            return Ok(Some(location));
        }
        let Reply::Location(location) = self
            .directory
            .request(&Request::Lookup { key: key.clone() })
            .await?
        else {
            anyhow::bail!(Failure::Unavailable);
        };
        if let Some(location) = location {
            location
                .object
                .validate()
                .map_err(|_| Failure::Unavailable)?;
            ensure!(key.belongs_to(location.object.kind), Failure::Unavailable);
            self.put(hash, Value::Location(location));
        }
        Ok(location)
    }
    pub async fn by_name(&self, name: &str) -> Result<Option<Arc<Cell>>> {
        let key = Key::new(KeyKind::Name, name).map_err(|_| Failure::Invalid)?;
        let Some(location) = self.location(&key).await? else {
            return Ok(None);
        };
        Ok(Some(
            self.cell(location.cell)
                .await?
                .ok_or(Failure::Unavailable)?,
        ))
    }
    pub async fn by_object(&self, object: Object) -> Result<Option<Arc<Cell>>> {
        object.validate().map_err(|_| Failure::Invalid)?;
        let key = RouteKey::Home(object.kind.as_str(), object.id);
        let cell = match self.get(key) {
            Some(Value::Home(cell)) => cell,
            _ => {
                let Reply::Home(cell) = self.directory.request(&Request::Home { object }).await?
                else {
                    anyhow::bail!(Failure::Unavailable);
                };
                let Some(cell) = cell else {
                    return Ok(None);
                };
                self.put(key, Value::Home(cell));
                cell
            }
        };
        Ok(Some(self.cell(cell).await?.ok_or(Failure::Unavailable)?))
    }
    /// Placement is never cached; the reservation checks the live cell again at commit.
    pub async fn place(&self, region: &str) -> Result<Option<Arc<Cell>>> {
        codeyard_model::cells::validate_region(region).map_err(|_| Failure::Invalid)?;
        let Reply::Cell(cell) = self
            .directory
            .request(&Request::Choose {
                region: region.to_owned(),
            })
            .await?
        else {
            anyhow::bail!(Failure::Unavailable);
        };
        let Some(cell) = cell else {
            return Ok(None);
        };
        let cell = cell.prepare().map_err(|_| Failure::Unavailable)?;
        ensure!(
            cell.region == region && cell.state == State::Open,
            Failure::Unavailable
        );
        Ok(Some(Arc::new(cell)))
    }
}

/// Keep the resource cell's credential cache warm without retaining the credential.
pub fn web_node<'a>(cell: &'a Cell, credential: &str) -> Result<&'a str> {
    ensure!(
        !cell.web_urls.is_empty() && credential.len() <= 4096,
        Failure::Invalid
    );
    let hash = token_hash(credential);
    let hash = u64::from_str_radix(&hash[..16], 16)?;
    let index = (hash % cell.web_urls.len() as u64) as usize;
    Ok(&cell.web_urls[index])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_cache_expires_and_never_grows_past_its_limits() {
        let config = Config {
            cache_seconds: 60,
            maximum_entries: 2,
            maximum_bytes: 1024,
        };
        let mut cache = Cache::new(config).unwrap();
        let now = Instant::now();
        let key = RouteKey::Cell(Prefix::ZERO);
        cache.put(key, Value::Home(Prefix::ZERO), now);
        assert!(cache.get(key, now + Duration::from_secs(59)).is_some());
        assert!(cache.get(key, now + Duration::from_secs(60)).is_none());
        for prefix in 1..1000 {
            cache.put(
                RouteKey::Cell(Prefix::new(prefix)),
                Value::Home(Prefix::new(prefix)),
                now,
            );
        }
        assert!(cache.entries.len() <= config.maximum_entries);
        assert!(cache.bytes <= config.maximum_bytes);
        let mut large = Cell {
            id: Prefix::ZERO,
            name: "a".repeat(2048),
            region: "a".into(),
            state: State::Open,
            web_urls: vec![],
            ssh_addresses: vec![],
            node_keys: vec![],
            database_bytes: 0,
            write_rate: 0,
            maximum_database_bytes: 1,
            maximum_write_rate: 1,
        };
        cache.put(key, Value::Cell(Arc::new(large.clone())), now);
        assert!(
            cache.get(key, now).is_none(),
            "one oversized record is not cached"
        );
        large.web_urls = vec!["https://one.example".into(), "https://two.example".into()];
        let selected = web_node(&large, "secret").unwrap();
        for _ in 0..10 {
            assert_eq!(web_node(&large, "secret").unwrap(), selected);
        }
        assert!(web_node(&large, &"x".repeat(4097)).is_err());
    }
}
