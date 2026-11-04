//! Content-addressed objects are reserved before storage I/O so interrupted writes are collected.
use crate::backend::{
    db::{
        self,
        packages::{Blob, Refusal},
    },
    storage::{ObjectStore, Upload, MAX_OBJECT_BYTES},
};
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};

pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

pub fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
}

pub fn valid_kind(kind: &str) -> bool {
    matches!(kind, "container" | "npm" | "maven" | "nuget" | "rubygems")
}

pub fn valid_package_name(kind: &str, name: &str) -> bool {
    if kind == "container" {
        return valid_name(name);
    }
    if kind == "npm" {
        return !name.starts_with('@')
            && !name.contains('/')
            && crate::backend::npm::package_name(name);
    }
    let part = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && s.bytes().any(|b| b.is_ascii_alphanumeric())
    };
    name.len() <= 255
        && match kind {
            "maven" => name
                .split_once(':')
                .is_some_and(|(group, artifact)| group.split('.').all(part) && part(artifact)),
            "nuget" => {
                name.len() <= 100
                    && part(name)
                    && name.split(['.', '-']).all(|s| {
                        !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
            }
            "rubygems" => part(name),
            _ => false,
        }
}

pub async fn store(
    pool: &codeyard_db_core::DbPool,
    storage: &ObjectStore,
    package: i64,
    token: i64,
    bytes: Vec<u8>,
    charge: db::packages::Charge<'_>,
) -> Result<Blob> {
    ensure!(bytes.len() <= MAX_OBJECT_BYTES as usize, Refusal::Blob);
    let io = std::sync::Arc::new(storage.try_acquire_io()?);
    let hashing_io = io.clone();
    let (hash, bytes) = tokio::task::spawn_blocking(move || {
        let _io = hashing_io;
        (digest(&bytes), bytes)
    })
    .await?;
    let blob = Blob {
        digest: hash,
        size: bytes.len() as i64,
    };
    let key = db::packages::object_key(&blob.digest);
    let _lock = storage.lock_file(&key).await?;
    if !db::packages::reserve_blob(pool, package, token, &blob, charge).await? {
        let upload = Upload {
            key: &key,
            bytes: blob.size,
            content_type: "application/octet-stream",
            sha256_hex: &blob.digest[7..],
        };
        // A cancelled request may have stored the immutable object before marking it ready.
        if storage.verify(&upload).await.is_err() {
            if let Err(error) = storage.put_bytes(&upload, bytes, io.clone()).await {
                // Another node can win S3's create-only PUT while this request is in flight.
                storage.verify(&upload).await.map_err(|_| error)?;
            }
        }
        db::packages::ready_blob(pool, package, token, &blob).await?;
    }
    Ok(blob)
}

pub async fn read(storage: &ObjectStore, blob: &Blob) -> Result<Vec<u8>> {
    ensure!((0..=MAX_OBJECT_BYTES).contains(&blob.size), Refusal::Blob);
    let io = std::sync::Arc::new(storage.try_acquire_io()?);
    let key = db::packages::object_key(&blob.digest);
    let bytes = storage.read_object(&key, blob.size as usize).await?;
    let (actual, bytes) = tokio::task::spawn_blocking(move || {
        let _io = io;
        (digest(&bytes), bytes)
    })
    .await?;
    ensure!(actual == blob.digest, "package object checksum mismatch");
    Ok(bytes)
}

pub async fn collect(pool: &codeyard_db_core::DbPool, storage: &ObjectStore) -> Result<()> {
    let Some(_collecting) = db::packages::collector_lock(pool).await? else {
        return Ok(());
    };
    for digest in db::packages::garbage(pool).await? {
        let key = db::packages::object_key(&digest);
        let result = async {
            let _lock = storage.lock_file(&key).await?;
            storage.delete(&key).await?;
            db::packages::removed_blob(pool, &digest).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%digest, %error, "package blob cleanup deferred");
        }
    }
    Ok(())
}
