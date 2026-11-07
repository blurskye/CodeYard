//! Deadlines for heartbeat-proved identity snapshots. These types do not establish that
//! a heartbeat and credential came from one snapshot; the database reader must do that.
//! Keep the resulting deadline across all subsequent waits, never start a new budget.
use anyhow::{ensure, Result};
use chrono::{DateTime, TimeDelta, Utc};
use std::time::{Duration, Instant};

/// Parse once; a malformed configuration cannot silently enable a weaker policy.
pub fn policy() -> Result<Policy> {
    static POLICY: std::sync::OnceLock<Result<Policy, String>> = std::sync::OnceLock::new();
    POLICY
        .get_or_init(|| Policy::from_env().map_err(|error| error.to_string()))
        .as_ref()
        .copied()
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

/// The clock-error allowance is included in, not added to, the published age bound.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    maximum_age: Duration,
    clock_error: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            maximum_age: Duration::from_secs(2),
            clock_error: Duration::from_millis(250),
        }
    }
}

impl Policy {
    pub fn new(maximum_age: Duration, clock_error: Duration) -> Result<Self> {
        ensure!(
            maximum_age > clock_error,
            "Invalid identity freshness budget"
        );
        // Validate conversion once, before this policy can be used by a request.
        TimeDelta::from_std(maximum_age)?;
        TimeDelta::from_std(clock_error)?;
        Ok(Self {
            maximum_age,
            clock_error,
        })
    }

    /// Configuration is read once by the identity reader, not per credential.
    pub fn from_env() -> Result<Self> {
        fn milliseconds(name: &str, default: u64) -> Result<Duration> {
            let value = match std::env::var(name) {
                Ok(value) => value.parse::<u64>()?,
                Err(std::env::VarError::NotPresent) => default,
                Err(error) => return Err(error.into()),
            };
            Ok(Duration::from_millis(value))
        }
        Self::new(
            milliseconds("IDENTITY_MAXIMUM_AGE_MS", 2000)?,
            milliseconds("IDENTITY_CLOCK_ERROR_MS", 250)?,
        )
    }

    pub fn maximum_age(self) -> Duration {
        self.maximum_age
    }

    /// Start before acquiring a connection or making a remote request. A slow primary
    /// response cannot start a fresh revocation budget when it finally arrives.
    pub fn start(self, wall: DateTime<Utc>, monotonic: Instant) -> Option<Read> {
        Some(Read {
            policy: self,
            started_wall: wall,
            started_monotonic: monotonic,
            request_deadline: monotonic.checked_add(self.maximum_age)?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Read {
    policy: Policy,
    started_wall: DateTime<Utc>,
    started_monotonic: Instant,
    request_deadline: Instant,
}

impl Read {
    /// Check immediately after each acquisition, RPC or database wait. A backward clock
    /// step larger than the configured clock allowance is an unavailable proof.
    pub fn current(self, wall: DateTime<Utc>, monotonic: Instant) -> bool {
        let Ok(allowance) = TimeDelta::from_std(self.policy.clock_error) else {
            return false;
        };
        monotonic >= self.started_monotonic
            && monotonic < self.request_deadline
            && wall
                .checked_add_signed(allowance)
                .is_some_and(|latest| latest >= self.started_wall)
    }

    /// Accept a heartbeat only together with its credential snapshot. An absent credential
    /// is a denial regardless of heartbeat age; callers must not turn absence into a hit.
    /// A stale/invalid heartbeat requires the primary or a service-unavailable response.
    pub fn prove(
        self,
        heartbeat: DateTime<Utc>,
        credential_expiry: Option<DateTime<Utc>>,
        wall: DateTime<Utc>,
        monotonic: Instant,
    ) -> Option<Deadline> {
        if !self.current(wall, monotonic) {
            return None;
        }
        let allowance = TimeDelta::from_std(self.policy.clock_error).ok()?;
        if heartbeat > wall.checked_add_signed(allowance)? {
            return None;
        }
        let usable = TimeDelta::from_std(self.policy.maximum_age - self.policy.clock_error).ok()?;
        let wall_deadline = heartbeat.checked_add_signed(usable)?;
        let wall_deadline = credential_expiry.map_or(wall_deadline, |expiry| {
            // The home clock may be ahead of this verifier by the same allowance.
            expiry
                .checked_sub_signed(allowance)
                .map_or(DateTime::<Utc>::MIN_UTC, |expiry| expiry.min(wall_deadline))
        });
        // Anchor to the clock sample taken before any wait, not to receipt time. Otherwise
        // a backward clock adjustment during a wait could extend an already-issued proof.
        let remaining = wall_deadline
            .signed_duration_since(self.started_wall)
            .to_std()
            .ok()?;
        let deadline = Deadline {
            read: self,
            wall: wall_deadline,
            monotonic: self
                .started_monotonic
                .checked_add(remaining)?
                .min(self.request_deadline),
        };
        deadline.current(wall, monotonic).then_some(deadline)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    read: Read,
    wall: DateTime<Utc>,
    monotonic: Instant,
}

impl Deadline {
    /// Upper bound for browser/SQL projections. Authority must also check `current`
    /// so a clock adjustment cannot bypass the monotonic deadline.
    pub fn wall(self) -> DateTime<Utc> {
        self.wall
    }

    /// Recheck after every wait, including waits for local resource row locks. SQL's
    /// predicate may have been evaluated before a lock was acquired.
    pub fn current(self, wall: DateTime<Utc>, monotonic: Instant) -> bool {
        self.read.current(wall, monotonic) && wall < self.wall && monotonic < self.monotonic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clocks() -> (DateTime<Utc>, Instant) {
        (
            DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            Instant::now(),
        )
    }

    #[test]
    fn allowance_is_inside_the_published_bound() {
        let (wall, monotonic) = clocks();
        let read = Policy::default().start(wall, monotonic).unwrap();
        let boundary = wall - TimeDelta::milliseconds(1750);
        assert!(read.prove(boundary, None, wall, monotonic).is_none());
        assert!(read
            .prove(boundary + TimeDelta::milliseconds(1), None, wall, monotonic)
            .is_some());
        assert!(read
            .prove(wall + TimeDelta::milliseconds(251), None, wall, monotonic)
            .is_none());
    }

    #[test]
    fn expired_credential_is_not_rescued_by_a_fresh_heartbeat() {
        let (wall, monotonic) = clocks();
        let read = Policy::default().start(wall, monotonic).unwrap();
        assert!(read.prove(wall, Some(wall), wall, monotonic).is_none());
        let deadline = read
            .prove(wall, Some(wall + TimeDelta::seconds(1)), wall, monotonic)
            .unwrap();
        assert!(!deadline.current(
            wall + TimeDelta::milliseconds(750),
            monotonic + Duration::from_millis(750)
        ));
    }

    #[test]
    fn a_wait_cannot_extend_the_deadline() {
        let (wall, monotonic) = clocks();
        let read = Policy::default().start(wall, monotonic).unwrap();
        let deadline = read.prove(wall, None, wall, monotonic).unwrap();
        assert!(deadline.current(
            wall + TimeDelta::seconds(1),
            monotonic + Duration::from_secs(1)
        ));
        // A row-lock wait crosses expiry; even a backward wall clock cannot save it.
        assert!(!deadline.current(wall, monotonic + Duration::from_secs(2)));
        assert!(read
            .prove(
                wall + TimeDelta::seconds(2),
                None,
                wall + TimeDelta::seconds(2),
                monotonic + Duration::from_secs(2)
            )
            .is_none());
    }

    #[test]
    fn clock_steps_fail_closed_without_refreshing_the_budget() {
        let (wall, monotonic) = clocks();
        let read = Policy::default().start(wall, monotonic).unwrap();
        assert!(read
            .prove(wall, None, wall - TimeDelta::seconds(1), monotonic)
            .is_none());
        let deadline = read.prove(wall, None, wall, monotonic).unwrap();
        assert!(!deadline.current(wall + TimeDelta::seconds(2), monotonic));
        let after_wait = monotonic + Duration::from_millis(1750);
        assert!(read
            .prove(wall, None, wall - TimeDelta::milliseconds(200), after_wait)
            .is_none());
    }

    #[test]
    fn policy_rejects_an_empty_or_unrepresentable_budget() {
        assert!(Policy::new(Duration::ZERO, Duration::ZERO).is_err());
        assert!(Policy::new(Duration::from_secs(1), Duration::from_secs(1)).is_err());
        assert!(Policy::new(Duration::MAX, Duration::ZERO).is_err());
        let policy = Policy::new(Duration::from_secs(3), Duration::from_millis(100)).unwrap();
        assert_eq!(policy.maximum_age(), Duration::from_secs(3));
    }
}
