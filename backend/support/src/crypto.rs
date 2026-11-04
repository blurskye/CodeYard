//! Random tokens and keyed hashes shared by native server code.
use rand::RngCore;

/// 32 random bytes from the operating system as 64 lowercase hex digits. This is the one
/// recipe for session ids, credentials, email-link tokens and salts; callers add any prefix.
pub fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// True when `value` is exactly `len` hex digits.
pub fn is_hex_token(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// HMAC-SHA256 bytes; callers own key encoding and wire prefixes.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(key)
        .expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}
