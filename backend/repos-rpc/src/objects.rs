//! Byte objects share authenticated node transport, but never Git/user authorization.
use crate::RpcHandler;
use codeyard_db_cluster::objects as db;
use codeyard_git::error::RepoError;
use codeyard_repos_core::wire::{invalid, unavailable, Operation, Request};
use codeyard_storage::{local::LocalStore, nodes::Declaration, Upload};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

fn declaration(object: db::Object) -> Result<Declaration, RepoError> {
    let declaration = Declaration {
        key: object.object_key,
        digest: object.digest,
        bytes: object.bytes,
        content_type: object.content_type,
    };
    if !declaration.valid() {
        return Err(unavailable());
    }
    Ok(declaration)
}

impl RpcHandler {
    pub(super) async fn object_admission(
        &self,
        request: &Request,
        caller: iroh::PublicKey,
    ) -> Result<Declaration, RepoError> {
        if !request.repository.is_nil() || request.actor.is_some() || request.ci_run.is_some() {
            return Err(invalid("invalid object RPC"));
        }
        let (key, write, delete) = match &request.operation {
            Operation::ObjectPut { declaration } if declaration.valid() => {
                (&declaration.key, true, false)
            }
            Operation::ObjectGet { key, .. } => (key, false, false),
            Operation::ObjectDelete { key } => (key, false, true),
            _ => return Err(invalid("invalid object RPC")),
        };
        if key.is_empty() || key.len() > 200 {
            return Err(invalid("invalid object key"));
        }
        let object = db::authorize(
            self.context.database()?,
            key,
            self.context.node_id,
            &self.context.key.public_hex(),
            &caller.to_string(),
            write,
            delete,
        )
        .await
        .map_err(|_| unavailable())?;
        let stored = declaration(object)?;
        if let Operation::ObjectPut { declaration } = &request.operation {
            if declaration.digest != stored.digest
                || declaration.bytes != stored.bytes
                || declaration.content_type != stored.content_type
            {
                return Err(invalid("object declaration changed"));
            }
        }
        Ok(stored)
    }

    pub(super) async fn execute_object(
        &self,
        request: Request,
        body: Vec<u8>,
        caller: iroh::PublicKey,
    ) -> Result<(serde_json::Value, Vec<u8>), RepoError> {
        static IO: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
        static LOCKS: std::sync::OnceLock<codeyard_support::locks::KeyedMutex<String>> =
            std::sync::OnceLock::new();
        let io = Arc::new(
            IO.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
                .clone()
                .try_acquire_owned()
                .map_err(|_| unavailable())?,
        );
        let declaration = self.object_admission(&request, caller).await?;
        let address = declaration.address().map_err(|_| unavailable())?;
        let root = self.context.repos_path.join("objects");
        let _lock = LOCKS
            .get_or_init(Default::default)
            .lock(root.join(&address).to_string_lossy().into_owned())
            .await;
        self.object_admission(&request, caller).await?;
        let store = LocalStore::open(&root).await.map_err(|_| unavailable())?;
        let pool = self.context.database()?;
        match &request.operation {
            Operation::ObjectPut { .. } => {
                let upload = Upload {
                    key: &address,
                    bytes: declaration.bytes,
                    content_type: &declaration.content_type,
                    sha256_hex: &declaration.digest,
                };
                store
                    .put(&upload, body.as_slice(), io)
                    .await
                    .map_err(|_| invalid("object bytes rejected"))?;
                db::ready(
                    pool,
                    &declaration.key,
                    self.context.node_id,
                    &self.context.key.public_hex(),
                    &caller.to_string(),
                )
                .await
                .map_err(|_| unavailable())?;
                Ok((serde_json::Value::Null, Vec::new()))
            }
            Operation::ObjectGet { offset, length, .. } => {
                if !body.is_empty() {
                    return Err(invalid("unexpected object body"));
                }
                let length = length.unwrap_or(declaration.bytes as u64);
                if offset
                    .checked_add(length)
                    .is_none_or(|end| end > declaration.bytes as u64)
                {
                    return Err(invalid("invalid object range"));
                }
                let (mut file, size) =
                    store.open_file(&address).await.map_err(|_| unavailable())?;
                if size != declaration.bytes as u64 {
                    return Err(unavailable());
                }
                file.seek(std::io::SeekFrom::Start(*offset))
                    .await
                    .map_err(|_| unavailable())?;
                let mut bytes = Vec::with_capacity((length as usize).min(64 * 1024));
                file.take(length)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|_| unavailable())?;
                if bytes.len() != length as usize {
                    return Err(unavailable());
                }
                self.object_admission(&request, caller).await?;
                Ok((
                    serde_json::to_value(declaration).map_err(|_| unavailable())?,
                    bytes,
                ))
            }
            Operation::ObjectDelete { .. } => {
                if !body.is_empty() {
                    return Err(invalid("unexpected object body"));
                }
                store
                    .delete_guarded(&address, (_lock, io))
                    .await
                    .map_err(|_| unavailable())?;
                db::forget_copy(
                    pool,
                    &declaration.key,
                    self.context.node_id,
                    &self.context.key.public_hex(),
                    &caller.to_string(),
                )
                .await
                .map_err(|_| unavailable())?;
                Ok((serde_json::Value::Null, Vec::new()))
            }
            _ => Err(invalid("invalid object RPC")),
        }
    }
}
