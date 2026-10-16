//! Shared fail-closed admission for HTML/API across nodes; PostgreSQL owns budgets.

use crate::backend::db::DbPool;
use sha2::{Digest, Sha256};

#[derive(Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Limited,
    Unavailable,
}

pub async fn admit_login(
    pool: &DbPool,
    user_id: Option<i64>,
    identity: &str,
) -> Result<(), AdmissionError> {
    let account = match user_id {
        Some(id) => format!("user:{id}"),
        None => format!(
            "unknown:{}",
            hex::encode(Sha256::digest(identity.trim().to_lowercase().as_bytes()))
        ),
    };
    let admitted = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::backend::db::users::admission::admit_password_attempt(pool, &account),
    )
    .await
    .map_err(|_| AdmissionError::Unavailable)?
    .map_err(|_| AdmissionError::Unavailable)?;
    if admitted {
        Ok(())
    } else {
        Err(AdmissionError::Limited)
    }
}
