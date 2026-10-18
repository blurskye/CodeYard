//! Repository-secret AES-256-GCM only; S3 objects do not use this encryptor.
//! Availability: absent key disables secret operations; a configured malformed
//! key is a startup error, not equivalent to absence. Recovery requires the exact
//! original master key plus ciphertext AND nonce backups. Losing it is irreversible.
//! Rotation: the current format has no key ID. Do not simply replace the env key.
//! Quiesce secret writes, decrypt with the old key, re-encrypt with fresh random
//! nonces/new key transactionally, verify restore, and retain the old key for old
//! backups. Online/key-ring rotation requires a separately reviewed DB upgrade.
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::RngCore;

pub struct SecretEncryptor {
    cipher: Aes256Gcm,
}

impl SecretEncryptor {
    pub fn from_env() -> Result<Self, SecretError> {
        Self::configured_from_env()?.ok_or(SecretError::MissingMasterKey)
    }

    /// Startup must propagate Err. Only NotPresent legitimately disables secrets.
    pub fn configured_from_env() -> Result<Option<Self>, SecretError> {
        match std::env::var("SECRETS_MASTER_KEY") {
            Ok(value) => Self::from_hex_key(&value).map(Some),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretError::InvalidMasterKey),
        }
    }

    pub fn from_hex_key(value: &str) -> Result<Self, SecretError> {
        if value.len() != 64 {
            return Err(SecretError::InvalidMasterKey);
        }
        let mut key = [0u8; 32];
        hex::decode_to_slice(value, &mut key).map_err(|_| SecretError::InvalidMasterKey)?;
        Ok(Self::new(&key))
    }

    pub fn new(master_key: &[u8; 32]) -> Self {
        let cipher = Aes256Gcm::new_from_slice(master_key).expect("valid key length");
        Self { cipher }
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, [u8; 12]), SecretError> {
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| SecretError::EncryptionFailed)?;

        Ok((ciphertext, nonce_bytes))
    }

    pub fn decrypt(&self, ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>, SecretError> {
        if nonce.len() != 12 {
            return Err(SecretError::InvalidNonce);
        }

        let nonce = Nonce::from_slice(nonce);
        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| SecretError::DecryptionFailed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("SECRETS_MASTER_KEY environment variable not set")]
    MissingMasterKey,
    #[error("Invalid master key format (must be 64 hex chars = 32 bytes)")]
    InvalidMasterKey,
    #[error("Encryption failed")]
    EncryptionFailed,
    #[error("Decryption failed")]
    DecryptionFailed,
    #[error("Invalid nonce")]
    InvalidNonce,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let key = [0x42u8; 32];
        let encryptor = SecretEncryptor::new(&key);

        let plaintext = b"super_secret_value";
        let (ciphertext, nonce) = encryptor.encrypt(plaintext).unwrap();

        assert_ne!(&ciphertext, plaintext);

        let decrypted = encryptor.decrypt(&ciphertext, &nonce).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    // NIST AES-256-GCM vector: all-zero key/96-bit IV/128-bit plaintext,
    // no AAD. Fixed input makes tag/tamper/wrong-key regressions deterministic.
    fn known_ciphertext() -> Vec<u8> {
        hex::decode("cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919").unwrap()
    }

    #[test]
    fn known_vector_and_every_ciphertext_bit_reject_tampering() {
        let encryptor = SecretEncryptor::new(&[0; 32]);
        let ciphertext = known_ciphertext();
        assert_eq!(encryptor.decrypt(&ciphertext, &[0; 12]).unwrap(), [0; 16]);
        for bit in 0..ciphertext.len() * 8 {
            let mut corrupted = ciphertext.clone();
            corrupted[bit / 8] ^= 1 << (bit % 8);
            assert!(matches!(
                encryptor.decrypt(&corrupted, &[0; 12]),
                Err(SecretError::DecryptionFailed)
            ));
        }
        for length in 0..ciphertext.len() {
            assert!(encryptor.decrypt(&ciphertext[..length], &[0; 12]).is_err());
        }
    }

    #[test]
    fn fixed_wrong_key_nonce_and_invalid_lengths_fail_closed() {
        let ciphertext = known_ciphertext();
        assert!(SecretEncryptor::new(&[1; 32])
            .decrypt(&ciphertext, &[0; 12])
            .is_err());
        let encryptor = SecretEncryptor::new(&[0; 32]);
        assert!(encryptor.decrypt(&ciphertext, &[1; 12]).is_err());
        for length in [0, 1, 11, 13, 32] {
            assert!(matches!(
                encryptor.decrypt(&ciphertext, &vec![0; length]),
                Err(SecretError::InvalidNonce)
            ));
        }
    }

    #[test]
    fn configured_key_parser_never_treats_invalid_as_disabled() {
        assert!(SecretEncryptor::from_hex_key(&"00".repeat(32)).is_ok());
        for value in [
            String::new(),
            "00".repeat(31),
            "00".repeat(33),
            "zz".repeat(32),
            format!(" {}", "00".repeat(32)),
        ] {
            assert!(matches!(
                SecretEncryptor::from_hex_key(&value),
                Err(SecretError::InvalidMasterKey)
            ));
        }
    }
}
