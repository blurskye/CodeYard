//! Canonical UTC persistence and exact monotonic discussion revisions.
use chrono::{DateTime, Duration, SecondsFormat, Utc};

pub(super) fn encode(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub(super) fn now() -> DateTime<Utc> {
    let value = Utc::now();
    DateTime::from_timestamp_micros(value.timestamp_micros())
        .expect("current UTC time is representable")
}

pub(super) fn next_revision(previous: DateTime<Utc>) -> Result<String, sqlx::Error> {
    let previous = previous
        .checked_add_signed(Duration::microseconds(1))
        .ok_or_else(|| sqlx::Error::Protocol("Discussion revision space exhausted".into()))?;
    Ok(encode(now().max(previous)))
}

/// Decode the stored revision without going through SQLite floating point date
/// arithmetic. The original raw string is retained and used in the UPDATE CAS.
pub(super) fn decode(value: &str) -> Result<DateTime<Utc>, sqlx::Error> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .map(|value| value.and_utc())
        })
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))
}
