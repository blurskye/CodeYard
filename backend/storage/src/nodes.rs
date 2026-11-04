//! Object byte transport supplied by the cluster, without a dependency on repository RPC.
use crate::{StorageError, Upload};
use futures_util::future::BoxFuture;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Declaration {
    pub key: String,
    pub digest: String,
    pub bytes: i64,
    pub content_type: String,
}

impl Declaration {
    pub fn upload(&self) -> Upload<'_> {
        Upload {
            key: &self.key,
            bytes: self.bytes,
            content_type: &self.content_type,
            sha256_hex: &self.digest,
        }
    }

    pub fn valid(&self) -> bool {
        crate::valid_key(&self.key)
            && (0..=codeyard_support::object_limits::MAX_OBJECT_BYTES).contains(&self.bytes)
            && self.digest.len() == 64
            && self
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && !self.content_type.is_empty()
            && self.content_type.len() <= 200
            && !self.content_type.chars().any(char::is_control)
    }

    /// Content and logical key both contribute to the immutable storage address. Deleting
    /// one reservation cannot remove another reservation's identical content.
    pub fn address(&self) -> Result<String, StorageError> {
        use sha2::Digest;
        if !self.valid() {
            return Err(StorageError::Invalid);
        }
        Ok(format!(
            "sha256/{}/{}",
            self.digest,
            hex::encode(sha2::Sha256::digest(self.key.as_bytes()))
        ))
    }
}

pub trait NodeStore: Send + Sync {
    fn put<'a>(
        &'a self,
        declaration: Declaration,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), StorageError>>;
    fn get<'a>(
        &'a self,
        key: &'a str,
        offset: u64,
        length: Option<u64>,
    ) -> BoxFuture<'a, Result<(Declaration, Vec<u8>), StorageError>>;
    fn verify<'a>(&'a self, declaration: Declaration) -> BoxFuture<'a, Result<(), StorageError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StorageError>>;
}

impl From<&Upload<'_>> for Declaration {
    fn from(upload: &Upload<'_>) -> Self {
        Self {
            key: upload.key.into(),
            digest: upload.sha256_hex.into(),
            bytes: upload.bytes,
            content_type: upload.content_type.into(),
        }
    }
}
