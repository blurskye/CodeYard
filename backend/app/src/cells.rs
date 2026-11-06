//! Cell RPC composition: identity owns proofs; repository stores own Git exports.
use crate::app::AppState;
use anyhow::{ensure, Context, Result};
use codeyard_db_core::{
    cells::{
        authorization::{Answer, Home, Proof},
        repositories::{Snapshot, SnapshotRequest},
    },
    models::User,
};
use futures_util::future::BoxFuture;

pub struct Native {
    pub state: AppState,
}
impl Native {
    fn identity(&self) -> codeyard_identity::cells::Local {
        codeyard_identity::cells::Local {
            pool: self.state.pool.clone(),
        }
    }
}
impl Home for Native {
    fn cell(&self) -> Option<codeyard_support::cells::Prefix> {
        Some(self.state.pool.cell_id())
    }
    fn authorize<'a>(&'a self, proof: &'a Proof) -> BoxFuture<'a, Result<Option<Answer>>> {
        Box::pin(async move { self.identity().authorize(proof).await })
    }
    fn user<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async move { self.identity().user(name).await })
    }
    fn user_id<'a>(&'a self, id: i64) -> BoxFuture<'a, Result<Option<User>>> {
        Box::pin(async move { self.identity().user_id(id).await })
    }
    fn receive_pointer<'a>(
        &'a self,
        event: &'a codeyard_db_core::cells::pointers::Event,
        admission: &'a dyn codeyard_db_core::cells::pointers::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.identity().receive_pointer(event, admission).await })
    }
    fn repository_source<'a>(
        &'a self,
        owner: &'a str,
        name: &'a str,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<codeyard_db_core::cells::repositories::Source>>> {
        Box::pin(async move { self.identity().repository_source(owner, name, proof).await })
    }
    fn snapshot<'a>(
        &'a self,
        request: &'a SnapshotRequest,
        output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<Snapshot>> {
        Box::pin(async move {
            request.validate()?;
            let maximum = codeyard_repos_core::repositories::store::fork_pack_bytes()?;
            let _slot = self
                .state
                .repository_downloads
                .clone()
                .try_acquire_owned()?;
            tokio::time::timeout(
                std::time::Duration::from_secs(request.timeout_seconds),
                async {
                    let identity = self.identity();
                    let source = identity
                        .repository_source(&request.owner, &request.name, &request.proof)
                        .await?
                        .context("Fork source is unavailable")?;
                    ensure!(
                        source.reader_id == request.reader_id
                            && source.repository.id == request.repository_id,
                        "Fork source or reader changed"
                    );
                    let actor = identity
                        .fork_actor(&request.proof)
                        .await?
                        .context("Fork credential was revoked")?;
                    let repository = codeyard_db_repos::repositories::creation::read_fork_source(
                        &self.state.pool,
                        &actor,
                        request.repository_id,
                    )
                    .await?
                    .context("Fork source was revoked")?;
                    let store = codeyard_repos_core::repositories::store::repo_store(
                        &repository,
                        self.state.cluster.as_ref(),
                    );
                    let mut bounded = codeyard_repos_core::git_transport::LimitedWriter::new(
                        output,
                        maximum.min(request.maximum_bytes),
                    );
                    let snapshot = store
                        .fork_snapshot(
                            &self.state.pool,
                            &actor,
                            repository.id,
                            request.default_branch_only,
                            codeyard_repos_core::store_types::GitStreams {
                                input: &mut tokio::io::empty(),
                                output: &mut bounded,
                                errors: &mut tokio::io::sink(),
                            },
                        )
                        .await?;
                    snapshot.validate(request)?;
                    let current = identity
                        .repository_source(&request.owner, &request.name, &request.proof)
                        .await?
                        .context("Fork source was revoked")?;
                    ensure!(
                        current.reader_id == request.reader_id
                            && current.repository.id == request.repository_id,
                        "Fork source or reader changed"
                    );
                    Ok(snapshot)
                },
            )
            .await?
        })
    }
}
