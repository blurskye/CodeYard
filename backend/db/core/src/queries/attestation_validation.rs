#[derive(Debug, thiserror::Error)]
pub enum AttestationError {
    #[error("Attestation database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Attestation authority unavailable")]
    Authority(#[source] anyhow::Error),
    #[error("Attestation access denied")]
    Denied,
    #[error("Attestation not found")]
    NotFound,
    #[error("Invalid Sigstore attestation bundle")]
    InvalidBundle,
    #[error("Attestation bundles must not exceed 1 MiB")]
    Size,
    #[error("Subject digests must be sha256 followed by 64 hexadecimal digits")]
    InvalidDigest,
    #[error("Invalid attestation pagination or predicate filter")]
    InvalidPage,
}

pub(crate) fn page_ids(rows: &[(i64, i64)], limit: usize) -> Result<Vec<i64>, AttestationError> {
    let mut bytes = 0usize;
    let mut ids = Vec::new();
    for &(id, size) in rows.iter().take(limit) {
        bytes = bytes.saturating_add(usize::try_from(size).map_err(|_| AttestationError::Size)?);
        if bytes > 8 * 1024 * 1024 {
            break;
        }
        ids.push(id);
    }
    if !rows.is_empty() && ids.is_empty() {
        return Err(AttestationError::Size);
    }
    Ok(ids)
}

pub fn subject_digest(value: &str) -> Result<String, AttestationError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(AttestationError::InvalidDigest);
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AttestationError::InvalidDigest);
    }
    Ok(format!("sha256:{}", hex.to_ascii_lowercase()))
}

pub struct Page<'a> {
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub limit: i64,
    pub predicate_type: Option<&'a str>,
}

impl Page<'_> {
    pub fn validate(&self) -> Result<(), AttestationError> {
        if !(1..=100).contains(&self.limit)
            || self.after.is_some_and(|id| id <= 0)
            || self.before.is_some_and(|id| id <= 0)
            || (self.after.is_some() && self.before.is_some())
            || self
                .predicate_type
                .is_some_and(|value| value.len() > 2048 || value.contains('\0'))
        {
            return Err(AttestationError::InvalidPage);
        }
        Ok(())
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct Attestation {
    pub id: i64,
    pub bundle: String,
    pub initiator: String,
}
