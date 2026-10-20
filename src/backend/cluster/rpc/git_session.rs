use super::{
    client::{attempt_deadline, cannot_serve, log_failover},
    wire::*,
    RpcHandler,
};
use crate::backend::{
    ci::{CiConfig, CiRuns},
    cluster::{link::Link, transport, NodeContext},
    git::repository::process as git_process,
    repositories::store::{GitService, GitStreams, RepoError, RepoStore},
};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A session the node accepted: the link it runs on and its first reply.
type Opened = (Link, Reply);

impl NodeContext {
    /// Runs a streamed operation on `node_id`. Returns the value the node put in its
    /// reply and the exit status of the stream. A read-only operation whose primary gave no
    /// reply is tried on its copies; that happens before any input or output has moved.
    pub async fn git_session(
        &self,
        node_id: i64,
        request: Request,
        streams: GitStreams<'_>,
    ) -> anyhow::Result<(Value, std::process::ExitStatus)> {
        use std::os::unix::process::ExitStatusExt;
        let receive = matches!(&request.operation, Operation::GitSession(session) if session.service == GitService::ReceivePack);
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let mut opened = self
            .open_session(
                node_id,
                &request,
                if request.operation.is_read_only() {
                    attempt_deadline(deadline)
                } else {
                    deadline
                },
            )
            .await;
        if opened.is_err() && request.operation.is_read_only() {
            let copies =
                tokio::time::timeout_at(deadline, self.read_copies(request.repository, node_id))
                    .await
                    .map_err(|_| unavailable())?;
            for (copy, moved) in copies {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                if copy == self.node_id {
                    // Admission comes first, so a copy this node cannot serve leaves the
                    // streams untouched for the next one.
                    if let Ok(Ok((store, checkout))) =
                        tokio::time::timeout_at(attempt_deadline(deadline), self.own_copy(&request))
                            .await
                    {
                        let Operation::GitSession(mut session) = request.operation else {
                            return Err(unavailable().into());
                        };
                        session.allow_unreachable = checkout;
                        let _permit = git_process::session_permit().await?;
                        // Boxed: the store's remote variant leads back to this function.
                        let status = Box::pin(store.git_session(session, None, streams)).await?;
                        return Ok((Value::Null, status));
                    }
                    continue;
                }
                let answer = self
                    .open_session(copy, &request, attempt_deadline(deadline))
                    .await;
                if !cannot_serve(answer.as_ref().ok().map(|(_, reply)| reply), moved) {
                    log_failover(request.repository, node_id, copy);
                    opened = answer;
                    break;
                }
            }
        }
        let (mut link, reply) = opened?;
        let value = reply.result.map_err(|failure| match failure {
            Failure::Repository(error) => error,
            Failure::Merge(_) => unavailable(),
        })?;
        let (activity, receiver) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let writer = &mut link.writer;
        let reader = &mut link.reader;
        let work = async {
            let inbound = async {
                let result =
                    git_process::copy(streams.input, &mut *writer, activity.clone(), false).await;
                let _ = writer.shutdown().await;
                result
            };
            let outbound = async {
                let mut disconnected = false;
                loop {
                    let bytes = read_frame(&mut *reader, git_process::CHUNK).await?;
                    if bytes.is_empty() {
                        break;
                    }
                    activity.send_replace(tokio::time::Instant::now());
                    if !disconnected {
                        match streams.output.write_all(&bytes).await {
                            Ok(()) => {
                                activity.send_replace(tokio::time::Instant::now());
                            }
                            Err(_) if receive => disconnected = true,
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                let reply: GitExit =
                    serde_json::from_slice(&read_frame(&mut *reader, MAX_JSON).await?)?;
                let _ = streams.errors.write_all(reply.stderr.as_bytes()).await;
                let code = reply.result?;
                Ok(std::process::ExitStatus::from_raw(code.clamp(0, 255) << 8))
            };
            tokio::pin!(inbound, outbound);
            tokio::select! {
                result = &mut outbound => result,
                result = &mut inbound => {
                    if let Err(error) = result {
                        if !receive { return Err(error); }
                    }
                    outbound.await
                }
            }
        };
        let result = git_process::session_timeout(work, receiver, git_process::IDLE_TIMEOUT).await;
        link.close(0, b"Git session complete");
        result.map(|status| (value, status))
    }

    /// The store of the copy this node holds. A node cannot dial itself, so the handler's
    /// own checks run in process. The caller already holds a session permit.
    async fn own_copy(&self, request: &Request) -> Result<(RepoStore, bool), RepoError> {
        let handler = RpcHandler::new(self.clone());
        let (store, repo_id) = handler
            .authorize_request(request, self.key.public_key())
            .await?;
        readable(&store).await?;
        let checkout = handler
            .checkout_access(request, repo_id, self.key.public_key())
            .await?;
        Ok((store, checkout))
    }

    /// Connects to `node_id`, sends the request and reads the first reply. `Err` means the
    /// node gave no reply at all. A reply that refuses the session comes back in `Ok`.
    async fn open_session(
        &self,
        node_id: i64,
        request: &Request,
        deadline: tokio::time::Instant,
    ) -> Result<Opened, RepoError> {
        let opened = tokio::time::timeout_at(deadline, async {
            let mut link = self.dial(node_id, transport::ALPN_RPC).await?;
            write_frame(&mut link.writer, &serde_json::to_vec(request)?, MAX_JSON).await?;
            let reply: Reply =
                serde_json::from_slice(&read_frame(&mut link.reader, MAX_JSON).await?)?;
            Ok::<_, anyhow::Error>((link, reply))
        })
        .await;
        match opened {
            Ok(Ok(opened)) => Ok(opened),
            _ => Err(unavailable()),
        }
    }
}

/// Check Git before admission, while failover can still try another node.
async fn readable(store: &RepoStore) -> Result<(), RepoError> {
    let path = store.local_path_for_spawning_git()?.to_owned();
    let permit = git_process::operation_slot(TIMEOUT)
        .await
        .map_err(RepoError::from)?;
    let opens = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        crate::backend::git::repository::opens_as_repository(&path)
    })
    .await
    .map_err(|_| unavailable())?
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
        let prepared = tokio::time::timeout_at(deadline, async {
            self.authorize_request(&request, caller).await?;
            let permit = git_process::session_permit()
                .await
                .map_err(RepoError::from)?;
            drop(admission);
            // The wait for the slot can outlast the membership or copy this request was
            // admitted on, so the same check runs again now.
            let (store, repo_id) = self.authorize_request(&request, caller).await?;
            readable(&store).await?;
            let checkout = self.checkout_access(&request, repo_id, caller).await?;
            let (admission, targets) = match &request.operation {
                // The copy learns HEAD and every ref now. The pack covers exactly these
                // refs, so a copy never points a ref at objects it does not have.
                Operation::ReplicaPack { .. } => {
                    let (refs, head) = store.replica_listing().await?;
                    let mut targets: Vec<String> = refs.values().cloned().collect();
                    targets.extend(crate::backend::db::ci::retained_ci_commits(self.context.database()?, repo_id.ok_or_else(unavailable)?).await.map_err(RepoError::from)?);
                    targets.sort();
                    targets.dedup();
                    (serde_json::json!({ "head": head, "refs": refs }), targets)
                }
                _ => (Value::Null, Vec::new()),
            };
            let receive = if matches!(&request.operation, Operation::GitSession(session) if session.service == GitService::ReceivePack) {
                let pool = self.context.database()?.clone();
                let ci = Arc::new(CiRuns::new(
                    pool.clone(),
                    Arc::new(CiConfig::from_env().map_err(RepoError::from)?),
                    Arc::new(tokio::sync::Notify::new()),
                    Some(self.context.clone()),
                    Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                        &pool,
                    )),
                ));
                Some((pool, ci))
            } else {
                None
            };
            Ok::<_, RepoError>((store, admission, targets, receive, permit, checkout))
        })
        .await.unwrap_or_else(|_| Err(unavailable()));
        let (store, admission, targets, receive, _permit, checkout) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                super::refuse(&mut link, error).await?;
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
                Operation::ReplicaPack { have } => {
                    store.replica_pack(&targets, &have, streams).await
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
