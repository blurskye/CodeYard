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
        Box::pin(async move {
            let identity = self.identity();
            let Some(source) = identity.repository_source(owner, name, proof).await? else {
                return Ok(None);
            };
            let repository = codeyard_db_repos::repositories::records::get_repository_by_id(
                &self.state.pool,
                source.repository.id,
            )
            .await?
            .context("Fork source disappeared")?;
            let reference = format!("refs/heads/{}", repository.default_branch);
            let oid = codeyard_repos_core::repositories::store::repo_store(
                &repository,
                self.state.cluster.as_ref(),
            )
            .matching_refs(&reference)
            .await?
            .remove(&reference);
            let Some(mut current) = identity.repository_source(owner, name, proof).await? else {
                return Ok(None);
            };
            ensure!(
                current.repository.id == source.repository.id
                    && current.repository.default_branch == source.repository.default_branch,
                "Fork source changed"
            );
            current.default_branch_oid = oid;
            current.validate(self.state.pool.cell_id())?;
            Ok(Some(current))
        })
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

/// Forward the original POST when its destination owner lives in another cell.
pub async fn forward_fork(
    state: &AppState,
    user: &User,
    target: Option<&str>,
    headers: &axum::http::HeaderMap,
    path: &str,
    body: Vec<u8>,
) -> codeyard_model::github_error::Result<Option<axum::response::Response>> {
    use axum::http::{header, StatusCode};
    use codeyard_model::{
        cells::{Key, KeyKind},
        github_error::Error,
    };
    let target = target.unwrap_or(&user.username);
    Key::new(KeyKind::Name, target).map_err(|_| Error::invalid("Invalid fork destination"))?;
    let local = if target == user.username {
        !user.is_stub && user.home_cell == i32::from(state.pool.cell_id().get())
    } else {
        codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, target)
            .await
            .map_err(Error::unavailable)?
            .is_some_and(|org| {
                !org.is_stub && org.home_cell == i32::from(state.pool.cell_id().get())
            })
    };
    if local {
        return Ok(None);
    }
    if headers.get_all(header::VIA).iter().any(|value| {
        value
            .to_str()
            .map_or(true, |value| value.contains("codeyard-fork"))
    }) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Fork destination route changed; retry the request",
        ));
    }
    let provider = state
        .pool
        .foreign_authorization()
        .ok_or_else(Error::missing)?;
    let mut forwarded = Vec::new();
    for (name, value) in headers.iter() {
        if codeyard_db_core::cells::repositories::request_header(name.as_str())
            && name != header::VIA
        {
            forwarded.push((
                name.as_str().to_owned(),
                value.to_str().map_err(Error::unavailable)?.to_owned(),
            ));
        }
    }
    forwarded.push(("via".to_owned(), "1.1 codeyard-fork".to_owned()));
    let request = codeyard_db_core::cells::repositories::ForwardFork {
        path: path.to_owned(),
        headers: forwarded,
        body,
    };
    request
        .validate()
        .map_err(|_| Error::invalid("Invalid fork forwarding request"))?;
    let reply = provider
        .forward_fork(target, &request)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let status = StatusCode::from_u16(reply.status).map_err(Error::unavailable)?;
    if !(200..=599).contains(&reply.status)
        || reply.body.len() > 8 * 1024 * 1024
        || reply.headers.len() > 128
    {
        return Err(Error::unavailable("Invalid fork destination response"));
    }
    let mut response = axum::response::Response::builder().status(status);
    for (name, value) in reply.headers {
        response = response.header(name, value);
    }
    response
        .body(axum::body::Body::from(reply.body))
        .map(Some)
        .map_err(Error::unavailable)
}

pub struct ForkSource {
    pub repository: codeyard_db_core::models::Repository,
    pub admission: Option<codeyard_db_core::cells::repositories::Admission>,
}
pub async fn fork_source(
    state: &AppState,
    actor: &codeyard_db_core::queries::credentials::Actor,
    credential: &str,
    owner: &str,
    name: &str,
) -> codeyard_model::github_error::Result<ForkSource> {
    use codeyard_model::github_error::Error;
    codeyard_model::cells::Key::new(codeyard_model::cells::KeyKind::Name, owner)
        .map_err(|_| Error::invalid("Invalid fork source"))?;
    if !codeyard_support::repository_names::valid(name) {
        return Err(Error::invalid("Invalid fork source"));
    }
    let proof =
        codeyard_db_core::cells::repositories::retained_proof(actor, credential).map_err(|_| {
            Error::new(
                axum::http::StatusCode::FORBIDDEN,
                "Fork requires its original credential",
            )
        })?;
    let local = codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, name)
        .await
        .map_err(Error::unavailable)?
        .filter(|repo| !repo.is_stub);
    if let Some(repository) = local {
        let source = codeyard_identity::cells::Local {
            pool: state.pool.clone(),
        }
        .repository_source(owner, name, &proof)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        if source.repository.id != repository.id {
            return Err(Error::unavailable("Fork source changed"));
        }
        return Ok(ForkSource {
            repository,
            admission: None,
        });
    }
    let provider = state
        .pool
        .foreign_authorization()
        .ok_or_else(Error::missing)?;
    let source = provider
        .repository_source(owner, name, &proof)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    source
        .retain(&state.pool)
        .await
        .map_err(Error::unavailable)?;
    let repository = source.repository.clone();
    let admission = codeyard_db_core::cells::repositories::Admission::new(
        source,
        actor.clone(),
        credential.to_owned(),
    )
    .map_err(|_| Error::new(axum::http::StatusCode::FORBIDDEN, "Fork credential changed"))?;
    Ok(ForkSource {
        repository,
        admission: Some(admission),
    })
}

/// A stub's metadata comes from its live resource home, under the original reader.
pub async fn foreign_repository(
    state: &AppState,
    actor: &codeyard_db_core::queries::credentials::Actor,
    repository: &codeyard_db_core::models::Repository,
) -> Result<Option<codeyard_db_core::cells::repositories::Source>> {
    ensure!(
        repository.is_stub && repository.home_cell != i32::from(state.pool.cell_id().get()),
        "Foreign repository has a native home"
    );
    let owner =
        codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, repository.id)
            .await?
            .context("Foreign repository owner is unavailable")?;
    let proof = codeyard_db_core::cells::repositories::actor_proof(actor)?;
    let provider = state
        .pool
        .foreign_authorization()
        .context("Foreign repository is unavailable")?;
    let Some(source) = provider
        .repository_source(&owner, &repository.name, &proof)
        .await?
    else {
        return Ok(None);
    };
    ensure!(
        source.repository.id == repository.id
            && source.repository.home_cell == repository.home_cell
            && source.reader_id == actor.user_id
            && source.credential_id == actor.token_id(),
        "Foreign repository identity changed"
    );
    Ok(Some(source))
}
