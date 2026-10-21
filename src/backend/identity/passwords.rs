//! Native password work: fixed reviewed cost, bounded PHC acceptance and no async-worker KDF.

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};
use rand::rngs::OsRng;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

pub const MAX_PASSWORD_BYTES: usize = 1024;
const MEMORY_KIB: u32 = 19_456;
const ITERATIONS: u32 = 2;
const PARALLELISM: u32 = 1;
// A stored hash may use any cost up to these limits, so raising the cost above does not lock out
// older hashes, while a corrupted hash cannot pick an expensive one.
const MAX_MEMORY_KIB: u32 = 65_536;
const MAX_ITERATIONS: u32 = 6;
const MAX_PARALLELISM: u32 = 4;
const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1$cHJvZHVjdGlvbi1kdW1teQ$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
static WORK: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Debug, PartialEq, Eq)]
pub enum PasswordError {
    InvalidInput,
    Busy,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Verification {
    pub valid: bool,
}

fn engine() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, Some(32))
            .expect("reviewed Argon2 parameters"),
    )
}

fn accepted_phc(stored: &str) -> Option<PasswordHash<'_>> {
    if stored.len() > 255 {
        return None;
    }
    let hash = PasswordHash::new(stored).ok()?;
    // The verifier uses PHC parameters, NOT the instance's parameters. Never accept
    // attacker/corruption-selected allocations or work factors. This lane emits one version.
    if hash.algorithm.as_str() != "argon2id"
        || hash.version != Some(19)
        || hash.params.iter().count() != 3
        || hash
            .params
            .iter()
            .any(|(name, _)| !matches!(name.as_str(), "m" | "t" | "p"))
        || hash.hash.as_ref()?.len() != 32
        || hash.salt.as_ref()?.as_str().len() > 64
    {
        return None;
    }
    let params = Params::try_from(&hash).ok()?;
    if params.m_cost() > MAX_MEMORY_KIB
        || params.t_cost() > MAX_ITERATIONS
        || params.p_cost() > MAX_PARALLELISM
    {
        return None;
    }
    Some(hash)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PasswordError> + Send + 'static,
) -> Result<T, PasswordError> {
    blocking_on(
        WORK.get_or_init(|| Arc::new(Semaphore::new(4))).clone(),
        work,
    )
    .await
}

async fn blocking_on<T: Send + 'static>(
    gate: Arc<Semaphore>,
    work: impl FnOnce() -> Result<T, PasswordError> + Send + 'static,
) -> Result<T, PasswordError> {
    let permit = gate.try_acquire_owned().map_err(|_| PasswordError::Busy)?;
    tokio::task::spawn_blocking(move || {
        // Cancellation of the awaiting task must NOT release a running KDF's permit.
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| PasswordError::Failed)?
}

pub async fn hash_password(password: &str) -> Result<String, PasswordError> {
    // Creation and change enforce the minimum length. Hashing accepts any password up to the
    // maximum length.
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(PasswordError::InvalidInput);
    }
    let password = password.as_bytes().to_vec();
    blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        engine()
            .hash_password(&password, &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| PasswordError::Failed)
    })
    .await
}

pub async fn verify_password(
    password: &str,
    stored: Option<&str>,
) -> Result<Verification, PasswordError> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(PasswordError::InvalidInput);
    }
    let password = password.as_bytes().to_vec();
    let stored = stored.filter(|value| value.len() <= 255).map(str::to_owned);
    blocking(move || {
        if let Some(hash) = stored.as_deref().and_then(accepted_phc) {
            return Ok(Verification {
                valid: engine().verify_password(&password, &hash).is_ok(),
            });
        }
        // Missing, OAuth-only and malformed hashes pay one normal KDF cost, so response time does
        // not show which accounts have a usable password.
        let dummy = PasswordHash::new(DUMMY).map_err(|_| PasswordError::Failed)?;
        let _ = engine().verify_password(&password, &dummy);
        Ok(Verification { valid: false })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    #[tokio::test]
    async fn salts_phc_and_negative_costs() {
        let a = hash_password("correct horse battery").await.unwrap();
        let b = hash_password("correct horse battery").await.unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(
            verify_password("correct horse battery", Some(&a))
                .await
                .unwrap()
                .valid
        );
        assert!(!verify_password("wrong", Some(&a)).await.unwrap().valid);
        assert!(accepted_phc(&a.replace("m=19456", "m=4294967295")).is_none());
        assert!(accepted_phc(&a.replace("t=2", "t=9000")).is_none());
        assert!(accepted_phc(&a.replace("p=1", "p=128")).is_none());
        assert!(accepted_phc(&a.replace("t=2", "t=3")).is_some());
        assert!(accepted_phc(&a.replace("argon2id", "argon2i")).is_none());
        assert!(accepted_phc(&a.replace("m=19456", "keyid=1234")).is_none());
        assert!(accepted_phc(DUMMY).is_some());
        assert!(!verify_password("wrong", None).await.unwrap().valid);
        assert!(
            !verify_password("wrong", Some("malformed"))
                .await
                .unwrap()
                .valid
        );
        assert!(
            !verify_password("wrong", Some(&a.replace("m=19456", "m=4294967295")))
                .await
                .unwrap()
                .valid
        );
        assert_eq!(
            hash_password(&"x".repeat(1025)).await,
            Err(PasswordError::InvalidInput)
        );
        assert_eq!(
            verify_password(&"x".repeat(1025), None).await,
            Err(PasswordError::InvalidInput)
        );
    }
    #[tokio::test]
    async fn other_hash_formats_fail_verification() {
        // Only Argon2id hashes are accepted. A salted SHA-256 digest must not verify, even with
        // the right password.
        let salt = [7_u8; 16];
        let mut sha = Sha256::new();
        sha.update(salt);
        sha.update(b"correct horse battery");
        let stored = format!("{}:{}", hex::encode(salt), hex::encode(sha.finalize()));
        assert!(
            !verify_password("correct horse battery", Some(&stored))
                .await
                .unwrap()
                .valid
        );
    }
    #[tokio::test]
    async fn blocking_cancellation_holds_permit_until_real_work_exits() {
        // An isolated semaphore proves the lifetime used by blocking(), without
        // competing with concurrently running cryptographic tests' global permits.
        let gate = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let work_gate = gate.clone();
        let task = tokio::spawn(async move {
            blocking_on(work_gate, move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
        });
        started_rx.await.unwrap();
        task.abort();
        assert_eq!(
            blocking_on(gate.clone(), || Ok(())).await,
            Err(PasswordError::Busy)
        );
        release_tx.send(()).unwrap();
        let _ = task.await;
        let _permit = gate.acquire_owned().await.unwrap();
    }
}
