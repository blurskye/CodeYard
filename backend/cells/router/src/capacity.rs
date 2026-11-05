//! Existing users do not depend on reports. Stale or failed reports close new placement.
use crate::directory::{Client, Reply, Request};
use anyhow::{ensure, Result};
use codeyard_db_core::{cells::capacity::Sample, DbPool};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct Config {
    pub sample_seconds: u64,
    pub request_seconds: u64,
    pub maximum_tables: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            sample_seconds: 5,
            request_seconds: 10,
            maximum_tables: 1024,
        }
    }
}
impl Config {
    fn validate(self) -> Result<()> {
        ensure!(
            (1..=10).contains(&self.sample_seconds)
                && (1..=60).contains(&self.request_seconds)
                && (1..=1024).contains(&self.maximum_tables),
            "Invalid cell capacity configuration"
        );
        Ok(())
    }
}

fn rate(previous: Option<(Sample, Instant)>, current: Sample, now: Instant) -> i64 {
    let Some((previous, then)) = previous else {
        return 0;
    };
    let elapsed = now.saturating_duration_since(then).as_nanos();
    if elapsed == 0 || current.writes < previous.writes {
        return i64::MAX;
    }
    u128::from(current.writes - previous.writes)
        .saturating_mul(1_000_000_000)
        .div_ceil(elapsed)
        .min(i64::MAX as u128) as i64
}

pub async fn run(
    pool: DbPool,
    directory: Client,
    config: Config,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    if config.validate().is_err() {
        tracing::warn!("Invalid cell capacity configuration");
        return;
    }
    let DbPool::Postgres(pool) = pool else {
        return;
    };
    let mut interval = tokio::time::interval(Duration::from_secs(config.sample_seconds));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous = None;
    loop {
        if *stop.borrow() {
            return;
        }
        tokio::select! { biased;
            _ = stop.changed() => return,
            _ = interval.tick() => {}
        }
        let result = tokio::select! { biased;
            _ = stop.changed() => return,
            result = tokio::time::timeout(Duration::from_secs(config.request_seconds), async {
                let sample = codeyard_db_core::cells::capacity::sample(&pool, config.maximum_tables).await?;
                let now = Instant::now();
                let write_rate = rate(previous, sample, now);
                let reply = directory.request(&Request::ReportLoad { database_bytes: sample.database_bytes, write_rate }).await?;
                ensure!(matches!(reply, Reply::Done), "Invalid directory capacity answer");
                Ok::<_, anyhow::Error>((sample, now))
            }) => result,
        };
        match result {
            Ok(Ok(sample)) => previous = Some(sample),
            _ => tracing::warn!("Cell capacity report unavailable; new placement will expire"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn write_rates_round_up_and_counter_resets_refuse_new_placement() {
        let now = Instant::now();
        let sample = |writes| Sample {
            database_bytes: 1,
            writes,
        };
        assert_eq!(
            rate(
                Some((sample(2), now)),
                sample(3),
                now + Duration::from_secs(2)
            ),
            1
        );
        assert_eq!(
            rate(
                Some((sample(3), now)),
                sample(2),
                now + Duration::from_secs(2)
            ),
            i64::MAX
        );
        assert_eq!(rate(Some((sample(2), now)), sample(3), now), i64::MAX);
    }
}
