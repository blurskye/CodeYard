//! Random tokens and keyed hashes shared by native server code.
use rand::TryRng;

/// 32 random bytes from the operating system as 64 lowercase hex digits. This is the one
/// recipe for session ids, credentials, email-link tokens and salts; callers add any prefix.
pub fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("operating system random source");
    hex::encode(bytes)
}

pub use codeyard_types::tokens::is_hex_token;

/// HMAC-SHA256 bytes; callers own key encoding and wire prefixes.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use hmac::{KeyInit, Mac};
    let mut mac = <hmac::Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key)
        .expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}
