//! Authenticated requests per account, router region and UTC day. Only requests a router
//! forwarded are counted, under the router's region. Counts stay in a bounded map and are
//! added to `cell_request_counts` once a minute; a failed write drops that minute.
use crate::pool::PgPool;
use anyhow::Result;
use chrono::NaiveDate;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Distinct (account, region, day) entries kept between writes; more are not counted.
pub const MAXIMUM_PENDING: usize = 100_000;
/// Rows written per statement.
const BATCH: usize = 1_000;
/// Credentials whose account is remembered, and for how long.
const MAXIMUM_IDENTITIES: usize = 10_000;
const IDENTITY_TIME: Duration = Duration::from_secs(60);
/// Counts older than this are deleted; the longest policy window fits inside it.
pub const RETENTION_DAYS: i32 = 60;

type Entry = (i64, String, NaiveDate);

#[derive(Default)]
pub struct Counts {
    pending: Mutex<HashMap<Entry, i64>>,
    identities: Mutex<HashMap<String, (Option<i64>, Instant)>>,
}
impl Counts {
    pub fn add(&self, user: i64, region: &str, day: NaiveDate) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let key = (user, region.to_owned(), day);
        if let Some(count) = pending.get_mut(&key) {
            *count = count.saturating_add(1);
        } else if pending.len() < MAXIMUM_PENDING {
            pending.insert(key, 1);
        }
    }
    /// The account a credential digest resolved to within the last minute: `Some(None)`
    /// for a credential that did not authenticate.
    pub fn identity(&self, credential: &str) -> Option<Option<i64>> {
        self.identities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(credential)
            .filter(|(_, at)| at.elapsed() < IDENTITY_TIME)
            .map(|(user, _)| *user)
    }
    pub fn remember(&self, credential: String, user: Option<i64>) {
        let mut identities = self
            .identities
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if identities.len() >= MAXIMUM_IDENTITIES {
            identities.retain(|_, (_, at)| at.elapsed() < IDENTITY_TIME);
        }
        if identities.len() < MAXIMUM_IDENTITIES {
            identities.insert(credential, (user, Instant::now()));
        }
    }
    fn take(&self) -> HashMap<Entry, i64> {
        std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }
}

/// Adds the pending counts and deletes counts past retention.
pub async fn flush(pool: &PgPool) -> Result<()> {
    let pending: Vec<(Entry, i64)> = pool.request_counts().take().into_iter().collect();
    for batch in pending.chunks(BATCH) {
        let mut users = Vec::with_capacity(batch.len());
        let mut regions = Vec::with_capacity(batch.len());
        let mut days = Vec::with_capacity(batch.len());
        let mut requests = Vec::with_capacity(batch.len());
        for ((user, region, day), count) in batch {
            users.push(*user);
            regions.push(region.clone());
            days.push(*day);
            requests.push(*count);
        }
        sqlx::query("INSERT INTO cell_request_counts(user_id,region,day,requests) SELECT * FROM UNNEST($1::bigint[],$2::text[],$3::date[],$4::bigint[]) ON CONFLICT(user_id,region,day) DO UPDATE SET requests=LEAST(cell_request_counts.requests+EXCLUDED.requests,9007199254740991)")
            .bind(users)
            .bind(regions)
            .bind(days)
            .bind(requests)
            .execute(pool)
            .await?;
    }
    sqlx::query("DELETE FROM cell_request_counts WHERE (user_id,region,day) IN (SELECT user_id,region,day FROM cell_request_counts WHERE day<(clock_timestamp() AT TIME ZONE 'UTC')::date-$1::int LIMIT 10000)")
        .bind(RETENTION_DAYS)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_bounded_and_identities_expire_from_the_map() {
        let counts = Counts::default();
        let day = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        counts.add(1, "b", day);
        counts.add(1, "b", day);
        for user in 0..(MAXIMUM_PENDING as i64 + 10) {
            counts.add(user + 2, "b", day);
        }
        let taken = counts.take();
        assert_eq!(taken.len(), MAXIMUM_PENDING);
        assert_eq!(taken[&(1, "b".to_owned(), day)], 2);
        assert!(counts.take().is_empty());
        counts.remember("a".into(), Some(7));
        counts.remember("b".into(), None);
        assert_eq!(counts.identity("a"), Some(Some(7)));
        assert_eq!(counts.identity("b"), Some(None));
        assert_eq!(counts.identity("c"), None);
    }
}
