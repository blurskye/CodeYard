use crate::context::NodeContext;
use crate::repositories::store::RepoStore;
use crate::wire::{Failure, Request};
use codeyard_git::error::RepoError;
use futures_util::future::BoxFuture;

pub trait RpcFactory: Send + Sync + std::fmt::Debug {
    fn create(&self, context: NodeContext) -> Box<dyn LocalRpc>;
}

pub trait LocalRpc: Send + Sync {
    fn execute(
        &self,
        request: Request,
        body: Vec<u8>,
        caller: iroh::PublicKey,
    ) -> BoxFuture<'_, Result<(serde_json::Value, Vec<u8>), Failure>>;
    fn own_copy<'a>(
        &'a self,
        request: &'a Request,
    ) -> BoxFuture<'a, Result<(RepoStore, bool, tokio::sync::OwnedRwLockReadGuard<()>), RepoError>>;
}
