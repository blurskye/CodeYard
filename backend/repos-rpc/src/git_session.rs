use crate::RpcHandler;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_ci::config::CiConfig;
use codeyard_ci_engine::runs::CiRuns;
use codeyard_cluster_core::link::Link;
use codeyard_db_core::limits::MAX_JSON;
use codeyard_git::error::RepoError;
use codeyard_git::repository::process as git_process;
use codeyard_repos_core::repositories::store::RepoStore;
use codeyard_repos_core::store_types::GitService;
use codeyard_repos_core::store_types::GitStreams;
use codeyard_repos_core::wire::unavailable;
use codeyard_repos_core::wire::write_frame;
use codeyard_repos_core::wire::GitExit;
use codeyard_repos_core::wire::Operation;
use codeyard_repos_core::wire::Reply;
use codeyard_repos_core::wire::Request;
use codeyard_repos_core::wire::TIMEOUT;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

use codeyard_repos_core::git_transport::{
    apply_refs, create_preparation, prepared_before, retain_write, write_targets, LimitedWriter,
    WriteLimits,
};
impl RpcHandler {
    pub(super) async fn prepare_write(
        &self,
        store: &RepoStore,
        id: &str,
        limits: WriteLimits,
    ) -> Result<()> {
        let path = store.local_path_for_spawning_git()?;
        let held = codeyard_git::locks::publication(path).await?;
        held.scope(async {
            let write = self.context.current_write(id).await?;
            ensure!(
                write.decision == "pending" && write.primary_id != self.context.node_id,
                "write is not pending on a follower"
            );
            let directory = create_preparation(path, &write).await?;
            if prepared_before(path, &write).await? {
                retain_write(path, &write, limits).await?;
                return self.context.vote_prepared_write(&write).await;
            }
            let mut pack = tokio::fs::File::create(directory.join("objects.pack")).await?;
            let repo = codeyard_db_repos::repositories::records::get_repository_by_id(
                self.context.database()?,
                write.repository_id,
            )
            .await?
            .context("repository disappeared")?;
            let repository = codeyard_repos_core::wire::repository_uuid(&repo.disk_path)?;
            let mut output = LimitedWriter::new(&mut pack, limits.pack_bytes);
            let (_, status) = self
                .context
                .git_session(
                    write.primary_id,
                    Request {
                        repository,
                        operation: Operation::WritePack {
                            id: write.id.clone(),
                        },
                        actor: None,
                        ci_run: None,
                    },
                    GitStreams {
                        input: &mut tokio::io::empty(),
                        output: &mut output,
                        errors: &mut tokio::io::sink(),
                    },
                )
                .await?;
            ensure!(status.success(), "write pack transfer failed");
            pack.sync_all().await?;
            retain_write(path, &write, limits).await?;
            self.context.vote_prepared_write(&write).await
        })
        .await
    }

    pub(super) async fn apply_write(
        &self,
        store: &RepoStore,
        id: &str,
        limits: WriteLimits,
    ) -> Result<()> {
        let path = store.local_path_for_spawning_git()?;
        let held = codeyard_git::locks::publication(path).await?;
        held.scope(async {
            let write = self.context.current_write(id).await?;
            if write.decision == "abort" {
                return self
                    .context
                    .remove_write_preparation(path, &write, limits)
                    .await;
            }
            ensure!(write.decision == "commit", "write is not committed");
            let view = codeyard_db_cluster::cluster::writes::recovery_view(
                self.context.database()?,
                write.repository_id,
            )
            .await?;
            let version = write.committed_version.context("write has no version")?;
            let applied = view
                .voters
                .iter()
                .find(|voter| voter.node_id == self.context.node_id)
                .context("voter disappeared")?
                .applied_version;
            if applied < version {
                ensure!(
                    applied == write.base_version,
                    "voter is not at the write's base version"
                );
                retain_write(path, &write, limits).await?;
                apply_refs(path, &write, limits.timeout).await?;
                self.context.publish_local_write(path, &write).await?;
            }
            self.context
                .remove_write_preparation(path, &write, limits)
                .await
        })
        .await
    }
}

/// Check Git before admission, while failover can still try another node.
async fn readable(store: &RepoStore) -> Result<(), RepoError> {
    let path = store.local_path_for_spawning_git()?.to_owned();
    let opens = git_process::blocking_until(std::time::Instant::now() + TIMEOUT, move || {
        codeyard_git::repository::opens_as_repository(&path)
    })
    .await
    .map_err(|_| unavailable())?;
    if opens {
        Ok(())
    } else {
        Err(unavailable())
    }
}

impl RpcHandler {
    pub(super) async fn serve_session(
        &self,
        request: Request,
        mut link: Link,
        deadline: tokio::time::Instant,
        admission: (
            tokio::sync::OwnedSemaphorePermit,
            tokio::sync::OwnedSemaphorePermit,
        ),
    ) -> Result<()> {
        let caller = link.peer;
        let write_pack = matches!(request.operation, Operation::WritePack { .. });
        let deadline = if write_pack {
            deadline.min(
                tokio::time::Instant::now()
                    + WriteLimits::load(self.context.database()?).await?.timeout,
            )
        } else {
            deadline
        };
        let prepared = tokio::time::timeout_at(deadline, async {
            self.authorize_request(&request, caller).await?;
            // A receive waiting for its voter already owns a session permit. Packs use
            // the existing bounded header admission instead, avoiding that circular wait.
            let permit = if write_pack { None } else { Some(git_process::session_permit().await.map_err(RepoError::from)?) };
            let pack_admission = if write_pack { Some(admission) } else { drop(admission); None };
            // The wait for the slot can outlast the membership or copy this request was
            // admitted on, so the same check runs again now.
            let (store, repo_id, read_guard) = if request.operation.is_read_only() {
                let admitted = self.read_admission(&request, caller).await?;
                (admitted.store, admitted.repo_id, Some(admitted.guard))
            } else {
                let (store, repo_id) = self.authorize_request(&request, caller).await?;
                (store, repo_id, None)
            };
            readable(&store).await?;
            self.authorize_request(&request, caller).await?;
            let checkout = self.checkout_access(&request, repo_id, caller).await?;
            if request.ci_run.is_some() && !checkout { return Err(unavailable()); }
            let (admission, targets) = match &request.operation {
                // The copy learns HEAD and every ref now. The pack covers exactly these
                // refs, so a copy never points a ref at objects it does not have.
                Operation::WritePack { id } => {
                    let write = codeyard_db_cluster::cluster::writes::load_write(self.context.database()?, id).await.map_err(RepoError::from)?.ok_or_else(unavailable)?;
                    (Value::Null, write_targets(&write).map_err(RepoError::from)?)
                }
                Operation::ReplicaPack { want,.. } => {
                    if want.len()>codeyard_git::repository::MAX_MIRROR_REFS || want.iter().any(|id|!codeyard_git::is_full_commit_id(id)) {
                        return Err(codeyard_repos_core::wire::invalid("invalid snapshot revisions"));
                    }
                    let (refs, head) = store.replica_listing().await?;
                    let mut targets: Vec<String> = refs.values().cloned().collect();
                    let requested=store.existing_commits(want).await?;
                    if want.iter().any(|id|!requested.contains(id)) {return Err(unavailable());}
                    targets.extend(want.iter().cloned());
                    let retained = codeyard_db_actions::ci::retained_ci_commits(self.context.database()?, repo_id.ok_or_else(unavailable)?).await.map_err(RepoError::from)?;
                    let present = store.existing_commits(&retained).await?;
                    targets.extend(retained.into_iter().filter(|commit| present.contains(commit)));
                    targets.extend(codeyard_db_repos::pull_requests::records::retained_revisions(self.context.database()?, repo_id.ok_or_else(unavailable)?).await.map_err(RepoError::from)?);
                    targets.sort();
                    targets.dedup();
                    (serde_json::json!({ "head": head, "refs": refs }), targets)
                }
                _ => (Value::Null, Vec::new()),
            };
            let receive: Option<(codeyard_db_core::DbPool, Arc<dyn codeyard_repos_core::receive_ci::ReceiveCi>)> = if matches!(&request.operation, Operation::GitSession(session) if session.service == GitService::ReceivePack) {
                let pool = self.context.database()?.clone();
                let ci = Arc::new(CiRuns::new(
                    pool.clone(),
                    Arc::new(CiConfig::from_env().map_err(RepoError::from)?),
                    Arc::new(tokio::sync::Notify::new()),
                    Some(self.context.clone()),
                    Arc::new(codeyard_notifications::broadcast::NotificationBroadcaster::new(
                        &pool,
                    )),
                ));
                Some((pool, ci))
            } else {
                None
            };
            Ok::<_, RepoError>((store, admission, targets, receive, permit, checkout, read_guard, pack_admission))
        })
        .await.unwrap_or_else(|_| Err(unavailable()));
        let (store, admission, targets, receive, _permit, checkout, _read_guard, _pack_admission) =
            match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    crate::refuse(&mut link, error).await?;
                    return Ok(());
                }
            };
        // The permit is held while the reply is written, so the write has a deadline too.
        tokio::time::timeout_at(
            deadline,
            write_frame(
                &mut link.writer,
                &serde_json::to_vec(&Reply {
                    result: Ok(admission),
                })?,
                MAX_JSON,
            ),
        )
        .await??;
        let (mut output, mut reader) = tokio::io::duplex(git_process::CHUNK);
        let receiving = receive.is_some();
        let input = &mut link.reader;
        let run = async move {
            let mut errors = git_process::StderrTail::default();
            let streams = GitStreams {
                input,
                output: &mut output,
                errors: &mut errors,
            };
            let status = match request.operation {
                Operation::GitSession(mut session) => {
                    session.allow_unreachable = checkout;
                    store.git_session(session, receive, streams).await
                }
                Operation::ReplicaPack { have, .. } => {
                    store.replica_pack(&targets, &have, streams).await
                }
                Operation::WritePack { .. } => {
                    async {
                        let limits = WriteLimits::load(self.context.database()?).await?;
                        let mut bounded = LimitedWriter::new(streams.output, limits.pack_bytes);
                        tokio::time::timeout(
                            limits.timeout,
                            store.replica_pack(
                                &targets,
                                &[],
                                GitStreams {
                                    input: streams.input,
                                    output: &mut bounded,
                                    errors: streams.errors,
                                },
                            ),
                        )
                        .await?
                    }
                    .await
                }
                _ => unreachable!("only streamed operations are served here"),
            };
            let result = status
                .map(|status| status.code().unwrap_or(1))
                .map_err(RepoError::from);
            GitExit {
                result,
                stderr: errors.text(),
            }
        };
        let outbound = {
            let send = &mut link.writer;
            // The block owns the reader, so returning from it makes Git's next write fail.
            async move {
                let mut buffer = vec![0; git_process::CHUNK];
                let mut disconnected = false;
                loop {
                    let count = reader.read(&mut buffer).await?;
                    if count == 0 {
                        break;
                    }
                    if !disconnected {
                        disconnected = !matches!(
                            tokio::time::timeout(
                                git_process::IDLE_TIMEOUT,
                                write_frame(send, &buffer[..count], git_process::CHUNK)
                            )
                            .await,
                            Ok(Ok(()))
                        );
                        // A lost upload-pack client stops the process. A receive keeps
                        // draining, because its hooks must finish.
                        if disconnected && !receiving {
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
        };
        let (reply, _) = tokio::join!(run, outbound);
        // Empty stdout frame separates binary chunks from the final JSON status. A peer
        // that stopped reading must not hold the session permit for ever.
        tokio::time::timeout(git_process::IDLE_TIMEOUT, async {
            write_frame(&mut link.writer, b"", git_process::CHUNK).await?;
            write_frame(&mut link.writer, &serde_json::to_vec(&reply)?, MAX_JSON).await?;
            link.finish().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("the caller stopped reading the session result"))?
    }
}

impl RpcHandler {
    /// The store of the copy this node holds. A node cannot dial itself, so the handler's
    /// own checks run in process. The caller already holds a session permit.
    pub(super) async fn own_copy(
        &self,
        request: &Request,
    ) -> Result<(RepoStore, bool, tokio::sync::OwnedRwLockReadGuard<()>), RepoError> {
        let handler = self;
        let admitted = handler
            .read_admission(request, self.context.key.public_key())
            .await?;
        readable(&admitted.store).await?;
        handler
            .authorize_request(request, self.context.key.public_key())
            .await?;
        let checkout = handler
            .checkout_access(request, admitted.repo_id, self.context.key.public_key())
            .await?;
        if request.ci_run.is_some() && !checkout {
            return Err(unavailable());
        }
        Ok((admitted.store, checkout, admitted.guard))
    }
}
