use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const MAX_FAILED_ATTEMPTS: u32 = 5;
const BAN_DURATION: Duration = Duration::from_secs(300);
const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct IpRecord {
    attempts: Vec<Instant>,
    banned_until: Option<Instant>,
}

#[derive(Clone)]
pub struct RateLimiter {
    records: Arc<RwLock<HashMap<IpAddr, IpRecord>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            records: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn is_blocked(&self, ip: IpAddr) -> bool {
        let records = self.records.read().await;
        if let Some(record) = records.get(&ip) {
            if let Some(banned_until) = record.banned_until {
                return Instant::now() < banned_until;
            }
        }
        false
    }

    pub async fn record_failure(&self, ip: IpAddr) -> bool {
        let mut records = self.records.write().await;
        let now = Instant::now();
        let cutoff = now - ATTEMPT_WINDOW;

        records.retain(|_, record| {
            record.attempts.retain(|attempt| *attempt > cutoff);
            if record.banned_until.is_some_and(|until| until <= now) {
                record.banned_until = None;
            }
            record.banned_until.is_some() || !record.attempts.is_empty()
        });

        let record = records.entry(ip).or_insert_with(|| IpRecord {
            attempts: Vec::new(),
            banned_until: None,
        });

        record.attempts.push(now);

        if record.attempts.len() as u32 >= MAX_FAILED_ATTEMPTS {
            record.banned_until = Some(now + BAN_DURATION);
            record.attempts.clear();
            return true;
        }

        false
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(last_octet: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, last_octet))
    }

    #[tokio::test]
    async fn failures_prune_expired_ips_and_preserve_active_records() {
        let limiter = RateLimiter::new();
        let now = Instant::now();
        let old_attempt = now - ATTEMPT_WINDOW - Duration::from_secs(1);
        let recent_attempt = now - Duration::from_secs(1);
        {
            let mut records = limiter.records.write().await;
            records.insert(
                ip(1),
                IpRecord {
                    attempts: vec![old_attempt],
                    banned_until: None,
                },
            );
            records.insert(
                ip(2),
                IpRecord {
                    attempts: Vec::new(),
                    banned_until: Some(now - Duration::from_secs(1)),
                },
            );
            records.insert(
                ip(3),
                IpRecord {
                    attempts: vec![old_attempt, recent_attempt],
                    banned_until: None,
                },
            );
            records.insert(
                ip(4),
                IpRecord {
                    attempts: Vec::new(),
                    banned_until: Some(now + BAN_DURATION),
                },
            );
            records.insert(
                ip(5),
                IpRecord {
                    attempts: vec![recent_attempt],
                    banned_until: Some(now - Duration::from_secs(1)),
                },
            );
        }

        assert!(!limiter.record_failure(ip(6)).await);
        assert!(limiter.is_blocked(ip(4)).await);
        assert!(!limiter.is_blocked(ip(5)).await);
        let records = limiter.records.read().await;
        assert_eq!(records.len(), 4);
        assert!(!records.contains_key(&ip(1)));
        assert!(!records.contains_key(&ip(2)));
        assert_eq!(records[&ip(3)].attempts, vec![recent_attempt]);
        assert!(records[&ip(5)].banned_until.is_none());
    }

    #[tokio::test]
    async fn failure_threshold_and_ban_survive_pruning() {
        let limiter = RateLimiter::new();
        for _ in 1..MAX_FAILED_ATTEMPTS {
            assert!(!limiter.record_failure(ip(1)).await);
            assert!(!limiter.is_blocked(ip(1)).await);
        }
        assert!(limiter.record_failure(ip(1)).await);
        assert!(!limiter.record_failure(ip(2)).await);
        assert!(limiter.is_blocked(ip(1)).await);
        assert!(!limiter.is_blocked(ip(2)).await);
    }
}
