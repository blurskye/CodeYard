//! Shared offset cursor encoding for native connections and bounded cell reads.
use base64::{engine::general_purpose::STANDARD, Engine};
pub fn encode(offset: i64) -> String {
    STANDARD.encode(format!("cursor:{offset}"))
}
pub fn decode(value: &str) -> Option<i64> {
    if value.len() > 64 {
        return None;
    }
    let decoded = STANDARD.decode(value).ok()?;
    let decoded = std::str::from_utf8(&decoded).ok()?;
    decoded
        .strip_prefix("cursor:")?
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0 && *n < i64::MAX)
}

#[cfg(test)]
mod tests {
    #[test]
    fn cursors_are_bounded_and_reject_invalid_offsets() {
        for offset in [0, 99, 10000, i64::MAX - 1] {
            assert_eq!(super::decode(&super::encode(offset)), Some(offset));
        }
        for value in [
            super::encode(-1),
            super::encode(i64::MAX),
            "a".repeat(65),
            "not base64".into(),
        ] {
            assert_eq!(super::decode(&value), None);
        }
    }
}
