//! Cell RPC composition: identity owns proofs; repository stores own Git exports.
use crate::app::AppState;
use anyhow::{ensure, Context, Result};
use codeyard_database::{
    cells::{
        authorization::{Answer, Home, Proof},
        pulls::{Binding, Grant, HeadRequest},
        repositories::{Snapshot, SnapshotRequest},
    },
    models::User,
};
use futures_util::future::BoxFuture;

mod search;

pub struct Native {
    pub state: AppState,
}
impl Native {
    fn identity(&self) -> codeyard_identity::cells::Local {
        codeyard_identity::cells::Local {
            pool: self.state.pool.clone(),
        }
    }
    /// The native fork, while its recorded publisher still has consent and read access.
    async fn consented_head(
        &self,
        request: &HeadRequest,
    ) -> Result<Option<codeyard_database::models::Repository>> {
        let pool = &self.state.pool;
        let binding = &request.binding;
        if codeyard_db_repos::pull_requests::cells::publisher(pool, binding).await?
            != Some(request.publisher_id)
        {
            return Ok(None);
        }
        let Some(repository) = codeyard_db_repos::repositories::creation::read_repository_source(
            pool,
            &codeyard_database::queries::credentials::Actor::from(request.publisher_id),
            binding.head_repo_id,
        )
        .await?
        .filter(|repository| repository.forked_from_id == Some(binding.base_repo_id)) else {
            return Ok(None);
        };
        if !codeyard_git_storage::repositories::maintainer_edit::publication_authorized(
            pool,
            Some(request.publisher_id),
            &repository,
        )
        .await?
        {
            return Ok(None);
        }
        Ok(Some(repository))
    }
    /// A maintainer's credential grants edits of this pull head only while the pull allows
    /// maintainer edits and the credential can still write the base.
    async fn maintainer(
        &self,
        proof: &Proof,
        base: &codeyard_database::models::Repository,
        pull: &codeyard_database::models::PullRequest,
    ) -> Result<Option<i64>> {
        if !pull.maintainer_edits_enabled() {
            return Ok(None);
        }
        let actor = if let Proof::Ssh { .. } = proof {
            // An SSH key acts as its whole account, as a native key does. Only a key homed
            // in this cell answers here.
            let Some(answer) = Home::authorize(&self.identity(), proof).await? else {
                return Ok(None);
            };
            codeyard_database::queries::credentials::Actor::from(answer.user.id)
        } else {
            let Some(actor) = self.identity().fork_actor(proof).await? else {
                return Ok(None);
            };
            actor
        };
        // Any failure, including an outage, refuses the edit.
        Ok(
            codeyard_git_storage::repositories::access::check_publication_actor(
                &self.state.pool,
                &actor,
                base,
            )
            .await
            .is_ok()
            .then_some(actor.user_id),
        )
    }
}

/// Push events from head cells move cross-cell pull heads, one pull request per delivery.
pub struct Refreshes {
    pub state: AppState,
}
impl codeyard_database::cells::outbox::Consumer for Refreshes {
    fn refresh_pull_head<'a>(
        &'a self,
        refresh: &'a mut codeyard_database::cells::pulls::Refresh,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let Some(pull) =
                codeyard_db_repos::pull_requests::cells::open(&self.state.pool, refresh, 1)
                    .await?
                    .into_iter()
                    .next()
            else {
                return Ok(true);
            };
            codeyard_ci::pull_requests::branches::follow_foreign_head(
                &self.state.pool,
                &self.state.ci_runs,
                &pull,
            )
            .await?;
            refresh.after = pull.id;
            Ok(false)
        })
    }
    fn index_search<'a>(
        &'a self,
        index: &'a dyn codeyard_database::cells::search::Index,
        repository: i64,
        item: codeyard_database::cells::search::Item,
        version: codeyard_database::cells::search::Version,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(search::send(&self.state, index, repository, item, version))
    }
}
impl Home for Native {
    fn cell(&self) -> Option<codeyard_types::cell_ids::Prefix> {
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
        event: &'a codeyard_database::cells::pointers::Event,
        admission: &'a dyn codeyard_database::cells::pointers::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.identity().receive_pointer(event, admission).await })
    }
    fn notice_public(&self, repository: i64) -> BoxFuture<'_, Result<bool>> {
        Box::pin(codeyard_database::cells::notices::public(
            &self.state.pool,
            repository,
        ))
    }
    fn receive_notice<'a>(
        &'a self,
        event: &'a codeyard_database::cells::notices::Event,
        admission: &'a dyn codeyard_database::cells::notices::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            codeyard_notifications::mentions::receive(
                &self.state.pool,
                &self.state.notification_broadcaster,
                event,
                admission,
            )
            .await
        })
    }
    fn receive_push<'a>(
        &'a self,
        event: &'a codeyard_database::cells::pulls::Event,
        admission: &'a dyn codeyard_database::cells::pulls::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            codeyard_db_repos::pull_requests::cells::receive_push(
                &self.state.pool,
                event,
                admission,
            )
            .await
        })
    }
    fn receive_pull<'a>(
        &'a self,
        binding: &'a Binding,
        proof: &'a Proof,
        admission: &'a dyn codeyard_database::cells::pulls::Admission,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let actor = self
                .identity()
                .fork_actor(proof)
                .await?
                .ok_or(codeyard_types::credentials::AccountRestricted)?;
            codeyard_db_repos::pull_requests::cells::subscribe(
                &self.state.pool,
                binding,
                &actor,
                admission,
            )
            .await
        })
    }
    /// The base answers for its own open pull request; it never lends a credential.
    fn pull_grant<'a>(
        &'a self,
        binding: &'a Binding,
        proof: Option<&'a Proof>,
    ) -> BoxFuture<'a, Result<Option<Grant>>> {
        Box::pin(async move {
            binding.validate()?;
            let pool = &self.state.pool;
            let native = i32::from(pool.cell_id().get());
            let Some(base) = codeyard_db_repos::repositories::records::get_repository_by_id(
                pool,
                binding.base_repo_id,
            )
            .await?
            .filter(|base| !base.is_stub && base.home_cell == native) else {
                return Ok(None);
            };
            let Some(pull) = codeyard_db_repos::pull_requests::records::get_pull_request(
                pool,
                base.id,
                binding.number,
            )
            .await?
            .filter(|pull| {
                pull.state == "open"
                    && pull.head_repo_id == Some(binding.head_repo_id)
                    && pull.head_branch == binding.head_branch
            }) else {
                return Ok(None);
            };
            let Some(publisher_id) = pull.head_publisher_id else {
                return Ok(None);
            };
            let actor_id = match proof {
                Some(proof) => self.maintainer(proof, &base, &pull).await?,
                None => None,
            };
            let grant = Grant {
                binding: binding.clone(),
                publisher_id,
                maintainer_edit_granted_by_id: pull
                    .maintainer_edits_enabled()
                    .then_some(pull.maintainer_edit_granted_by_id)
                    .flatten(),
                actor_id,
            };
            grant.validate(binding)?;
            Ok(Some(grant))
        })
    }
    /// The head exports one branch only while the consent its publisher recorded here
    /// still matches and that publisher can still read the fork.
    fn pull_snapshot<'a>(
        &'a self,
        request: &'a HeadRequest,
        output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> BoxFuture<'a, Result<Option<Snapshot>>> {
        Box::pin(async move {
            request.validate()?;
            let maximum = codeyard_git_storage::repositories::store::fork_pack_bytes()?;
            let _slot = self
                .state
                .repository_downloads
                .clone()
                .try_acquire_owned()?;
            tokio::time::timeout(
                std::time::Duration::from_secs(request.timeout_seconds),
                async {
                    let pool = &self.state.pool;
                    let binding = &request.binding;
                    let Some(repository) = self.consented_head(request).await? else {
                        return Ok(None);
                    };
                    let store = codeyard_git_storage::repositories::store::repo_store(
                        &repository,
                        self.state.cluster.as_ref(),
                    );
                    let reference = format!("refs/heads/{}", binding.head_branch);
                    if !store
                        .matching_refs(&reference)
                        .await?
                        .contains_key(&reference)
                    {
                        return Ok(None);
                    }
                    let mut bounded = codeyard_git_storage::git_transport::LimitedWriter::new(
                        output,
                        maximum.min(request.maximum_bytes),
                    );
                    let snapshot = store
                        .pull_snapshot(
                            pool,
                            request.publisher_id,
                            repository.id,
                            &binding.head_branch,
                            codeyard_git_storage::store_types::GitStreams {
                                input: &mut tokio::io::empty(),
                                output: &mut bounded,
                                errors: &mut tokio::io::sink(),
                            },
                        )
                        .await?;
                    request.validate_snapshot(&snapshot)?;
                    ensure!(
                        self.consented_head(request).await?.is_some(),
                        "Pull head consent changed"
                    );
                    Ok(Some(snapshot))
                },
            )
            .await?
        })
    }
    fn repository_id<'a>(
        &'a self,
        id: i64,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::Source>>> {
        Box::pin(async move {
            let Some(source) = self.identity().repository_id(id, proof).await? else {
                return Ok(None);
            };
            Ok(self
                .repository_source(source.owner.name(), &source.repository.name, proof)
                .await?
                .filter(|source| source.repository.id == id))
        })
    }
    fn repository_fork<'a>(
        &'a self,
        owner: &'a str,
        base: i64,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::Source>>> {
        Box::pin(async move {
            let Some(source) = self.identity().repository_fork(owner, base, proof).await? else {
                return Ok(None);
            };
            Ok(self
                .repository_source(owner, &source.repository.name, proof)
                .await?
                .filter(|source| source.repository.forked_from_id == Some(base)))
        })
    }
    fn repository_source<'a>(
        &'a self,
        owner: &'a str,
        name: &'a str,
        proof: &'a Proof,
    ) -> BoxFuture<'a, Result<Option<codeyard_database::cells::repositories::Source>>> {
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
            let oid = codeyard_git_storage::repositories::store::repo_store(
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
            let maximum = codeyard_git_storage::repositories::store::fork_pack_bytes()?;
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
                    let repository =
                        codeyard_db_repos::repositories::creation::read_repository_source(
                            &self.state.pool,
                            &actor,
                            request.repository_id,
                        )
                        .await?
                        .context("Fork source was revoked")?;
                    let store = codeyard_git_storage::repositories::store::repo_store(
                        &repository,
                        self.state.cluster.as_ref(),
                    );
                    let mut bounded = codeyard_git_storage::git_transport::LimitedWriter::new(
                        output,
                        maximum.min(request.maximum_bytes),
                    );
                    let snapshot = store
                        .fork_snapshot(
                            &self.state.pool,
                            &actor,
                            repository.id,
                            request.default_branch_only,
                            request.branch.as_deref(),
                            codeyard_git_storage::store_types::GitStreams {
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
) -> codeyard_types::github_error::Result<Option<axum::response::Response>> {
    use axum::http::{header, StatusCode};
    use codeyard_types::{
        cells::{Key, KeyKind},
        github_error::Error,
    };
    let target_kind = if target.is_some() {
        codeyard_types::cells::ObjectKind::Organization
    } else {
        codeyard_types::cells::ObjectKind::User
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
        if codeyard_database::cells::repositories::request_header(name.as_str())
            && name.as_str() != "via"
        {
            forwarded.push((
                name.as_str().to_owned(),
                value
                    .to_str()
                    .map_err(|_| Error::invalid("Invalid fork forwarding headers"))?
                    .to_owned(),
            ));
        }
    }
    forwarded.push(("via".to_owned(), "1.1 codeyard-fork".to_owned()));
    let request = codeyard_database::cells::repositories::ForwardFork {
        target_kind,
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
    pub repository: codeyard_database::models::Repository,
    pub admission: Option<codeyard_database::cells::repositories::Admission>,
}
pub async fn fork_source(
    state: &AppState,
    actor: &codeyard_database::queries::credentials::Actor,
    credential: &str,
    owner: &str,
    name: &str,
) -> codeyard_types::github_error::Result<ForkSource> {
    use codeyard_types::github_error::Error;
    codeyard_types::cells::Key::new(codeyard_types::cells::KeyKind::Name, owner)
        .map_err(|_| Error::invalid("Invalid fork source"))?;
    if !codeyard_types::repository_names::valid(name) {
        return Err(Error::invalid("Invalid fork source"));
    }
    let proof = codeyard_database::cells::repositories::retained_proof(actor, credential).map_err(
        |_| {
            Error::new(
                axum::http::StatusCode::FORBIDDEN,
                "Fork requires its original credential",
            )
        },
    )?;
    let local = codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, name)
        .await
        .map_err(Error::unavailable)?
        .filter(|repo| !repo.is_stub);
    if let Some(repository) = local {
        // Local forks need repository read access, not an exportable owner profile.
        let source = codeyard_db_repos::repositories::creation::read_repository_source(
            &state.pool,
            actor,
            repository.id,
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        if source.name != repository.name
            || source.owner_kind != repository.owner_kind
            || source.owner_user_id != repository.owner_user_id
            || source.owner_org_id != repository.owner_org_id
        {
            return Err(Error::unavailable("Fork source changed"));
        }
        return Ok(ForkSource {
            repository: source,
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
    retained_source(state, actor, credential, source).await
}

async fn retained_source(
    state: &AppState,
    actor: &codeyard_database::queries::credentials::Actor,
    credential: &str,
    source: codeyard_database::cells::repositories::Source,
) -> codeyard_types::github_error::Result<ForkSource> {
    use codeyard_types::github_error::Error;
    let admission = codeyard_database::cells::repositories::Admission::new(
        source,
        actor.clone(),
        credential.to_owned(),
    )
    .map_err(|_| {
        Error::new(
            axum::http::StatusCode::FORBIDDEN,
            "Repository credential changed",
        )
    })?;
    admission
        .source
        .retain(&state.pool)
        .await
        .map_err(Error::unavailable)?;
    Ok(ForkSource {
        repository: admission.source.repository.clone(),
        admission: Some(admission),
    })
}

/// Resolve a renamed foreign fork while keeping the base as the mutation's owner.
pub async fn pull_head_source(
    state: &AppState,
    actor: &codeyard_database::queries::credentials::Actor,
    credential: &str,
    base: &codeyard_database::models::Repository,
    owner: &str,
    name: Option<&str>,
) -> codeyard_types::github_error::Result<ForkSource> {
    use codeyard_types::github_error::Error;
    codeyard_types::cells::Key::new(codeyard_types::cells::KeyKind::Name, owner)
        .map_err(|_| Error::invalid("Invalid head owner"))?;
    if let Some(name) = name {
        return fork_source(state, actor, credential, owner, name).await;
    }
    if let Some(repo) =
        codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, &base.name)
            .await
            .map_err(Error::unavailable)?
            .filter(|repo| {
                !repo.is_stub && (repo.id == base.id || repo.forked_from_id == Some(base.id))
            })
    {
        return fork_source(state, actor, credential, owner, &repo.name).await;
    }
    if let Some(repo) =
        codeyard_db_repos::repositories::discovery::named_owner_fork(&state.pool, owner, base.id)
            .await
            .map_err(Error::unavailable)?
    {
        return fork_source(state, actor, credential, owner, &repo.name).await;
    }
    let proof = codeyard_database::cells::repositories::retained_proof(actor, credential)
        .map_err(Error::unavailable)?;
    let source = state
        .pool
        .foreign_authorization()
        .ok_or_else(Error::missing)?
        .repository_fork(owner, base.id, &proof)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    retained_source(state, actor, credential, source).await
}

pub async fn pull_head_id(
    state: &AppState,
    actor: &codeyard_database::queries::credentials::Actor,
    credential: &str,
    id: i64,
) -> codeyard_types::github_error::Result<ForkSource> {
    use codeyard_types::github_error::Error;
    codeyard_types::cell_ids::Prefix::for_id(id).map_err(|_| Error::missing())?;
    if let Some(repo) =
        codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .filter(|repo| !repo.is_stub)
    {
        let owner =
            codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        return fork_source(state, actor, credential, &owner, &repo.name).await;
    }
    let proof = codeyard_database::cells::repositories::retained_proof(actor, credential)
        .map_err(Error::unavailable)?;
    let source = state
        .pool
        .foreign_authorization()
        .ok_or_else(Error::missing)?
        .repository_id(id, &proof)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    retained_source(state, actor, credential, source).await
}

/// A stub's metadata comes from its live resource home, under the original reader.
pub async fn foreign_repository(
    state: &AppState,
    actor: &codeyard_database::queries::credentials::Actor,
    repository: &codeyard_database::models::Repository,
) -> Result<Option<codeyard_database::cells::repositories::Source>> {
    ensure!(
        repository.is_stub && repository.home_cell != i32::from(state.pool.cell_id().get()),
        "Foreign repository has a native home"
    );
    let owner =
        codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, repository.id)
            .await?
            .context("Foreign repository owner is unavailable")?;
    let proof = codeyard_database::cells::repositories::actor_proof(actor)?;
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
