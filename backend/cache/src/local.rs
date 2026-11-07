use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::time::Instant;

struct Entry {
    key: String,
    value: Arc<[u8]>,
    expires: Instant,
}

/// Front is most recently used. Both count and bytes include keys and values.
pub(crate) struct Local {
    entries: VecDeque<Entry>,
    bytes: usize,
    maximum_entries: usize,
    maximum_bytes: usize,
}
impl Local {
    pub fn new(maximum_entries: usize, maximum_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            maximum_entries,
            maximum_bytes,
        }
    }
    fn remove_at(&mut self, index: usize) -> Entry {
        let entry = self.entries.remove(index).expect("existing cache index");
        self.bytes -= entry.key.len() + entry.value.len();
        entry
    }
    pub fn remove(&mut self, key: &str) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.remove_at(index);
        }
    }
    pub fn get(&mut self, key: &str) -> Option<Arc<[u8]>> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        let entry = self.remove_at(index);
        if entry.expires <= Instant::now() {
            return None;
        }
        let result = entry.value.clone();
        self.bytes += entry.key.len() + entry.value.len();
        self.entries.push_front(entry);
        Some(result)
    }
    pub fn put(&mut self, key: String, value: Arc<[u8]>, ttl: Duration) {
        self.remove(&key);
        let size = key.len() + value.len();
        if ttl.is_zero() || self.maximum_entries == 0 || size > self.maximum_bytes {
            return;
        }
        while self.entries.len() >= self.maximum_entries || self.bytes + size > self.maximum_bytes {
            self.remove_at(self.entries.len() - 1);
        }
        self.bytes += size;
        self.entries.push_front(Entry {
            key,
            value,
            expires: Instant::now() + ttl,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn lru_is_bounded_by_count_bytes_and_expiry() {
        let mut cache = Local::new(2, 8);
        let value: Arc<[u8]> = Arc::from(&b"12"[..]);
        cache.put("a".into(), value.clone(), Duration::from_secs(1));
        cache.put("b".into(), value.clone(), Duration::from_secs(1));
        assert!(cache.get("a").is_some());
        cache.put("c".into(), value.clone(), Duration::from_secs(1));
        assert!(cache.get("b").is_none());
        cache.put(
            "big".into(),
            Arc::from(&b"too large"[..]),
            Duration::from_secs(1),
        );
        assert!(cache.get("big").is_none());
        cache.put("a".into(), value, Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(3)).await;
        assert!(cache.get("a").is_none());
        assert!(cache.get("c").is_some());
    }
}
