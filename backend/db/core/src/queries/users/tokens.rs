use chrono::DateTime;
use chrono::Utc;
/// Token timestamps must round-trip through SQLite's four-digit calendar representation.
pub fn supported_expiry(expiry: Option<DateTime<Utc>>) -> bool {
    expiry.is_none_or(|expiry| (1..=9999).contains(&expiry.year()))
}
