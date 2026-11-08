//! Shared-file placement and byte exchange over the existing node RPC transport.
use crate::{
    context::NodeContext,
    wire::{Operation, Request},
};
use codeyard_db_placement::objects as db;
use codeyard_storage::{
    nodes::{Declaration, NodeStore},
    StorageError,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

pub struct Store {
    context: NodeContext,
    slots: tokio::sync::Semaphore,
}
impl Store {
    pub fn new(context: NodeContext) -> Arc<Self> {
        Arc::new(Self {
            context,
            slots: tokio::sync::Semaphore::new(2),
        })
    }
    fn pool(&self) -> Result<&codeyard_database::DbPool, StorageError> {
        self.context
            .database()
            .map_err(|_| StorageError::Unavailable)
    }
    async fn call(
        &self,
        node: i64,
        operation: Operation,
        bytes: &[u8],
    ) -> Result<(serde_json::Value, Vec<u8>), StorageError> {
        let (reply, body) = self
            .context
            .call(
                node,
                Request {
                    repository: uuid::Uuid::nil(),
                    operation,
                    actor: None,
                    ci_run: None,
                },
                bytes,
            )
            .await
            .map_err(|_| StorageError::Unavailable)?;
        Ok((reply.result.map_err(|_| StorageError::Unavailable)?, body))
    }
    async fn declaration(&self, key: &str) -> Result<Declaration, StorageError> {
        let object = db::get(self.pool()?, key)
            .await
            .map_err(|_| StorageError::Unavailable)?
            .filter(|object| object.confirmed == 1)
            .ok_or(StorageError::NotFound)?;
        let declaration = Declaration {
            key: object.object_key,
            digest: object.digest,
            bytes: object.bytes,
            content_type: object.content_type,
        };
        if !declaration.valid() {
            return Err(StorageError::Rejected);
        }
        Ok(declaration)
    }
}
impl NodeStore for Store {
    fn put<'a>(
        &'a self,
        declaration: Declaration,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), StorageError>> {
        Box::pin(async move {
            use sha2::Digest;
            let _slot = self
                .slots
                .try_acquire()
                .map_err(|_| StorageError::Unavailable)?;
            if !declaration.valid()
                || bytes.len() != declaration.bytes as usize
                || hex::encode(sha2::Sha256::digest(&bytes)) != declaration.digest
                || !codeyard_storage::media_signature_matches(&declaration.content_type, &bytes)
            {
                return Err(StorageError::Rejected);
            }
            let record = db::Object {
                object_key: declaration.key.clone(),
                digest: declaration.digest.clone(),
                bytes: declaration.bytes,
                content_type: declaration.content_type.clone(),
                confirmed: 0,
            };
            let targets = db::reserve(
                self.pool()?,
                self.context.node_id,
                &self.context.key.public_hex(),
                &record,
            )
            .await
            .map_err(|_| StorageError::Unavailable)?;
            for node in targets {
                self.call(
                    node,
                    Operation::ObjectPut {
                        declaration: declaration.clone(),
                    },
                    &bytes,
                )
                .await?;
            }
            db::confirm(
                self.pool()?,
                self.context.node_id,
                &self.context.key.public_hex(),
                &declaration.key,
            )
            .await
            .map_err(|_| StorageError::Unavailable)
        })
    }
    fn get<'a>(
        &'a self,
        key: &'a str,
        offset: u64,
        length: Option<u64>,
    ) -> BoxFuture<'a, Result<(Declaration, Vec<u8>), StorageError>> {
        Box::pin(async move {
            use sha2::Digest;
            let _slot = self
                .slots
                .try_acquire()
                .map_err(|_| StorageError::Unavailable)?;
            let declaration = self.declaration(key).await?;
            let length = length.unwrap_or(declaration.bytes as u64);
            if offset
                .checked_add(length)
                .is_none_or(|end| end > declaration.bytes as u64)
            {
                return Err(StorageError::Invalid);
            }
            let copies = db::copies(self.pool()?, key)
                .await
                .map_err(|_| StorageError::Unavailable)?;
            for (node, ready) in copies {
                if ready != 1 {
                    continue;
                }
                let Ok((metadata, bytes)) = self
                    .call(
                        node,
                        Operation::ObjectGet {
                            key: key.into(),
                            offset,
                            length: Some(length),
                        },
                        &[],
                    )
                    .await
                else {
                    continue;
                };
                let Ok(actual) = serde_json::from_value::<Declaration>(metadata) else {
                    continue;
                };
                if actual.key != declaration.key
                    || actual.digest != declaration.digest
                    || actual.bytes != declaration.bytes
                    || actual.content_type != declaration.content_type
                    || bytes.len() != length as usize
                {
                    continue;
                }
                if offset == 0
                    && length == declaration.bytes as u64
                    && hex::encode(sha2::Sha256::digest(&bytes)) != declaration.digest
                {
                    continue;
                }
                return Ok((declaration, bytes));
            }
            Err(StorageError::Unavailable)
        })
    }
    fn verify<'a>(&'a self, declaration: Declaration) -> BoxFuture<'a, Result<(), StorageError>> {
        Box::pin(async move {
            let (actual, _) = self.get(&declaration.key, 0, None).await?;
            if actual.digest != declaration.digest
                || actual.bytes != declaration.bytes
                || actual.content_type != declaration.content_type
            {
                return Err(StorageError::Rejected);
            }
            Ok(())
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StorageError>> {
        Box::pin(async move {
            let _slot = self
                .slots
                .try_acquire()
                .map_err(|_| StorageError::Unavailable)?;
            if !db::begin_delete(
                self.pool()?,
                self.context.node_id,
                &self.context.key.public_hex(),
                key,
            )
            .await
            .map_err(|_| StorageError::Unavailable)?
            {
                return Ok(());
            }
            let copies = db::copies(self.pool()?, key)
                .await
                .map_err(|_| StorageError::Unavailable)?;
            for (node, _) in copies {
                self.call(node, Operation::ObjectDelete { key: key.into() }, &[])
                    .await?;
            }
            db::finish_delete(self.pool()?, key)
                .await
                .map_err(|_| StorageError::Unavailable)
        })
    }
}

/// A bounded pass fills reserved copies and retires draining copies only after two
/// active replacements are ready. The same object RPC validates every byte and caller.
pub async fn maintain(context: &NodeContext) -> anyhow::Result<()> {
    use sha2::Digest;
    let pool = context.database()?;
    let node = codeyard_db_placement::cluster::get_node_by_id(pool, context.node_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("object storage node disappeared"))?;
    if node.status != "active" || !node.roles.iter().any(|role| role == "storage") {
        return Ok(());
    }
    db::purge(pool).await?;
    let store = Store::new(context.clone());
    for (key, ready) in db::work(pool, node.id).await? {
        let retiring = if ready < 0 {
            true
        } else if node.state == "draining" {
            db::retire(pool, node.id, &context.key.public_hex(), &key).await?
        } else {
            false
        };
        if retiring {
            store
                .call(node.id, Operation::ObjectDelete { key }, &[])
                .await?;
            continue;
        }
        if ready != 0 || node.state != "active" {
            continue;
        }
        let object = db::get(pool, &key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("object placement disappeared"))?;
        if object.confirmed < 0 {
            continue;
        }
        let declaration = Declaration {
            key: key.clone(),
            digest: object.digest,
            bytes: object.bytes,
            content_type: object.content_type,
        };
        anyhow::ensure!(declaration.valid(), "invalid stored object declaration");
        let mut copied = false;
        for (source, current) in db::copies(pool, &key).await? {
            if source == node.id || current != 1 {
                continue;
            }
            let Ok((metadata, bytes)) = store
                .call(
                    source,
                    Operation::ObjectGet {
                        key: key.clone(),
                        offset: 0,
                        length: None,
                    },
                    &[],
                )
                .await
            else {
                continue;
            };
            let Ok(actual) = serde_json::from_value::<Declaration>(metadata) else {
                continue;
            };
            if actual.key != declaration.key
                || actual.digest != declaration.digest
                || actual.bytes != declaration.bytes
                || actual.content_type != declaration.content_type
                || bytes.len() != declaration.bytes as usize
                || hex::encode(sha2::Sha256::digest(&bytes)) != declaration.digest
            {
                continue;
            }
            store
                .call(
                    node.id,
                    Operation::ObjectPut {
                        declaration: declaration.clone(),
                    },
                    &bytes,
                )
                .await?;
            copied = true;
            break;
        }
        anyhow::ensure!(
            copied,
            "object replacement is waiting for a readable source"
        );
    }
    Ok(())
}
