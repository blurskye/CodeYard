use crate::context::NodeContext;
use crate::wire::invalid;
use crate::wire::operation_body_limit;
use crate::wire::read_frame;
use crate::wire::unavailable;
use crate::wire::write_frame;
use crate::wire::Failure;
use crate::wire::Operation;
use crate::wire::Reply;
use crate::wire::Request;
use crate::wire::MAX_BYTES;
use crate::wire::TIMEOUT;
use anyhow::Result;
use codeyard_cluster_core::link::Link;
use codeyard_cluster_core::transport::NodeAddress;
use codeyard_db_core::limits::MAX_JSON;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

impl NodeContext {
    /// The caller holds the publication lease and repeats live authority after preparation.
    pub(crate) async fn publish_ref_changes<F>(
        &self,
        repository: Uuid,
        path: &std::path::Path,
        repo: i64,
        term: i64,
        changes: &[codeyard_db_core::models::RefChange],
        authorize: F,
    ) -> anyhow::Result<()>
    where
        F: std::future::Future<Output = anyhow::Result<()>>,
    {
        use codeyard_db_cluster::cluster::writes as journal;
        let pool = self.database()?;
        let limits = crate::git_transport::WriteLimits::load(pool).await?;
        let id = Uuid::new_v4().to_string();
        let write =
            codeyard_cluster_core::writes::begin(pool, repo, self.node_id, term, &id, changes)
                .await?;
        let prepared = self
            .prepare_push_write(repository, path, write.record())
            .await;
        let votes = prepared.is_ok()
            && codeyard_cluster_core::writes::collect_votes(pool, &write)
                .await
                .unwrap_or(false);
        let authorized = if votes {
            tokio::time::timeout(std::time::Duration::from_secs(5), authorize)
                .await
                .is_ok_and(|result| result.is_ok())
        } else {
            false
        };
        let decided =
            codeyard_cluster_core::writes::decide(pool, &write, votes && authorized).await?;
        if decided.decision != "commit" {
            if self
                .abort_push_write(repository, path, &decided)
                .await
                .is_ok()
            {
                journal::finish_write(pool, &decided.id, decided.term).await?;
            }
            anyhow::bail!("write quorum unavailable");
        }
        // After the SQL decision every failure is an unknown outcome, never a rollback.
        if crate::git_transport::apply_refs(path, &decided, limits.timeout)
            .await
            .is_err()
            || self
                .publish_push_write(repository, path, &decided)
                .await
                .is_err()
        {
            return Err(codeyard_cluster_core::writes::OutcomeUnknown.into());
        }
        Ok(())
    }

    pub(crate) async fn prepare_push_write(
        &self,
        repository: Uuid,
        path: &std::path::Path,
        write: &codeyard_db_core::models::WriteRecord,
    ) -> anyhow::Result<()> {
        let limits = crate::git_transport::WriteLimits::load(self.database()?).await?;
        tokio::time::timeout(limits.timeout, async {
            let follower = self.write_follower(write).await?;
            self.prepare_local_write(path, write, limits).await?;
            let (reply, _) = self
                .call(
                    follower,
                    Request {
                        repository,
                        operation: Operation::PrepareWrite {
                            id: write.id.clone(),
                        },
                        actor: None,
                        ci_run: None,
                    },
                    &[],
                )
                .await?;
            reply.result.map_err(|_| unavailable())?;
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    pub(crate) async fn publish_push_write(
        &self,
        repository: Uuid,
        path: &std::path::Path,
        write: &codeyard_db_core::models::WriteRecord,
    ) -> anyhow::Result<()> {
        let limits = crate::git_transport::WriteLimits::load(self.database()?).await?;
        tokio::time::timeout(limits.timeout, async {
            let follower = self.write_follower(write).await?;
            self.publish_local_write(path, write).await?;
            let (reply, _) = self
                .call(
                    follower,
                    Request {
                        repository,
                        operation: Operation::ApplyWrite {
                            id: write.id.clone(),
                        },
                        actor: None,
                        ci_run: None,
                    },
                    &[],
                )
                .await?;
            reply.result.map_err(|_| unavailable())?;
            self.remove_write_preparation(path, write, limits).await?;
            codeyard_cluster_core::writes::publish(self.database()?, write).await?;
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    pub(crate) async fn abort_push_write(
        &self,
        repository: Uuid,
        path: &std::path::Path,
        write: &codeyard_db_core::models::WriteRecord,
    ) -> anyhow::Result<()> {
        let limits = crate::git_transport::WriteLimits::load(self.database()?).await?;
        tokio::time::timeout(
            limits.timeout.min(std::time::Duration::from_secs(1)),
            async {
                anyhow::ensure!(write.decision == "abort", "write is not aborted");
                let follower = self.write_follower(write).await?;
                self.remove_write_preparation(path, write, limits).await?;
                let (reply, _) = self
                    .call(
                        follower,
                        Request {
                            repository,
                            operation: Operation::ApplyWrite {
                                id: write.id.clone(),
                            },
                            actor: None,
                            ci_run: None,
                        },
                        &[],
                    )
                    .await?;
                reply.result.map_err(|_| unavailable())?;
                Ok::<_, anyhow::Error>(())
            },
        )
        .await?
    }

    async fn write_follower(
        &self,
        write: &codeyard_db_core::models::WriteRecord,
    ) -> anyhow::Result<i64> {
        let view = codeyard_db_cluster::cluster::writes::recovery_view(
            self.database()?,
            write.repository_id,
        )
        .await?;
        anyhow::ensure!(
            view.primary_id == self.node_id
                && write.primary_id == self.node_id
                && view.term == write.term
                && view.voters.len() == 2,
            "write term changed"
        );
        view.voters
            .iter()
            .find(|voter| voter.node_id != self.node_id && voter.term == write.term)
            .map(|voter| voter.node_id)
            .ok_or_else(|| anyhow::anyhow!("write voter missing"))
    }

    pub async fn call(
        &self,
        node_id: i64,
        request: Request,
        body: &[u8],
    ) -> Result<(Reply, Vec<u8>), RepoError> {
        self.call_with_response_limit(node_id, request, body, MAX_BYTES)
            .await
    }

    pub(crate) async fn call_with_response_limit(
        &self,
        node_id: i64,
        request: Request,
        body: &[u8],
        response_body_limit: usize,
    ) -> Result<(Reply, Vec<u8>), RepoError> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        tokio::time::timeout_at(deadline, async {
            if body.len() > operation_body_limit(&request.operation) {
                return Err(invalid("RPC frame exceeds its limit"));
            }
            let header =
                serde_json::to_vec(&request).map_err(|_| invalid("invalid RPC request"))?;
            if header.len() > MAX_JSON {
                return Err(invalid("RPC frame exceeds its limit"));
            }
            let first = self
                .exchange(
                    node_id,
                    &header,
                    body,
                    response_body_limit,
                    if request.operation.is_read_only() {
                        attempt_deadline(deadline)
                    } else {
                        deadline
                    },
                )
                .await;
            if !request.operation.is_read_only()
                || !cannot_serve(first.as_ref().ok().map(|(reply, _)| reply), true)
            {
                return first;
            }
            let mut candidates = self
                .read_copies(request.repository, node_id)
                .await
                .into_iter()
                .peekable();
            while let Some((copy, moved)) = candidates.next() {
                let candidate_deadline = if candidates.peek().is_none() {
                    deadline
                } else {
                    attempt_deadline(deadline)
                };
                let answer = self
                    .exchange(copy, &header, body, response_body_limit, candidate_deadline)
                    .await;
                if !cannot_serve(answer.as_ref().ok().map(|(reply, _)| reply), moved) {
                    log_failover(request.repository, node_id, copy);
                    return answer;
                }
            }
            first
        })
        .await
        .map_err(|_| unavailable())?
    }

    /// One attempt leaves half the remaining request budget for failover.
    async fn exchange(
        &self,
        node_id: i64,
        header: &[u8],
        body: &[u8],
        response_body_limit: usize,
        deadline: tokio::time::Instant,
    ) -> Result<(Reply, Vec<u8>), RepoError> {
        if node_id == self.node_id {
            return tokio::time::timeout_at(
                deadline,
                self.exchange_here(header, body, response_body_limit),
            )
            .await
            .map_err(|_| unavailable())?;
        }
        let mut link = tokio::time::timeout_at(
            deadline,
            self.dial(node_id, codeyard_cluster_core::transport::ALPN_RPC),
        )
        .await
        .map_err(|_| unavailable())??;
        let result = tokio::time::timeout_at(deadline, async {
            let sending = async {
                write_frame(&mut link.writer, header, MAX_JSON).await?;
                write_frame(&mut link.writer, body, MAX_BYTES).await?;
                link.writer.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            };
            let receiving = async {
                let reply: Reply =
                    serde_json::from_slice(&read_frame(&mut link.reader, MAX_JSON).await?)?;
                let body = if reply.result.is_err() {
                    Vec::new()
                } else {
                    read_frame(&mut link.reader, response_body_limit).await?
                };
                Ok::<_, anyhow::Error>((reply, body))
            };
            tokio::pin!(sending, receiving);
            tokio::select! {
                biased;
                reply = &mut receiving => reply,
                _ = &mut sending => receiving.await,
            }
        })
        .await;
        link.close(0, b"RPC complete");
        result
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())
    }

    /// The answer of this node's own handler. A node cannot dial itself, so a read that
    /// falls back to a copy held here is served in process, with the same checks.
    // The handler can reach receive hooks that call back here. Explicit Send breaks
    // that recursive proof when the receive broker spawns its hook tasks.
    fn exchange_here<'a>(
        &'a self,
        header: &'a [u8],
        body: &'a [u8],
        response_body_limit: usize,
    ) -> futures_util::future::BoxFuture<'a, Result<(Reply, Vec<u8>), RepoError>> {
        Box::pin(async move {
            let request: Request = serde_json::from_slice(header).map_err(|_| unavailable())?;
            let handler = self.rpc.create(self.clone());
            let (result, body) = match handler
                .execute(request, body.to_vec(), self.key.public_key())
                .await
            {
                Ok((value, body)) => (Ok(value), body),
                Err(failure) => (Err(failure), Vec::new()),
            };
            if body.len() > response_body_limit {
                return Err(unavailable());
            }
            Ok((Reply { result }, body))
        })
    }

    /// Opens a request stream to `node_id` for `alpn`. A node whose last dial failed is
    /// refused at once for a few seconds.
    pub(super) fn dial<'a>(
        &'a self,
        node_id: i64,
        alpn: &'a [u8],
    ) -> futures_util::future::BoxFuture<'a, Result<Link, RepoError>> {
        Box::pin(async move {
            if self.recently_unreachable(node_id) {
                return Err(unavailable());
            }
            let (node, address) = if let Some(pool) = &self.pool {
                let node = codeyard_db_cluster::cluster::get_node_by_id(pool, node_id)
                    .await
                    .map_err(|_| unavailable())?
                    .filter(|node| {
                        node.status == "active" && node.roles.iter().any(|role| role == "storage")
                    })
                    .ok_or_else(unavailable)?;
                let address: NodeAddress =
                    serde_json::from_str(node.addr_hint.as_deref().ok_or_else(unavailable)?)
                        .map_err(|_| unavailable())?;
                let peer = codeyard_support::node_identity::parse_public_key(&node.public_key)
                    .map_err(|_| unavailable())?;
                (peer, address)
            } else {
                let peer = self
                    .peers
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&node_id)
                    .cloned()
                    .ok_or_else(unavailable)?;
                let address = peer.resolved_address().await.map_err(|_| unavailable())?;
                (peer.node().map_err(|_| unavailable())?, address)
            };
            let linked =
                codeyard_cluster_core::transport::dial_node(&self.iroh, node, &address, alpn).await;
            self.note_dial(node_id, linked.is_ok());
            linked.map_err(|_| unavailable())
        })
    }

    /// The nodes that may answer a read after `primary` gave no reply, best first: the
    /// repository's primary if it moved while the request waited, then online storage nodes
    /// other than `primary` whose copy has applied the repository's current write version.
    /// Only looked up after such a failure. Each node is paired with
    /// whether it is the primary that moved.
    pub async fn read_copies(&self, repository: Uuid, primary: i64) -> Vec<(i64, bool)> {
        let Ok(pool) = self.database() else {
            return self
                .ci_replicas
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&repository)
                .into_iter()
                .flatten()
                .copied()
                .filter(|id| *id != primary)
                .map(|id| (id, false))
                .collect();
        };
        codeyard_db_cluster::cluster::read_candidates(pool, &repository.to_string(), primary)
            .await
            .unwrap_or_default()
    }
}

/// Reserves time for another candidate even when a connected peer never responds.
pub fn attempt_deadline(deadline: tokio::time::Instant) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now + deadline.saturating_duration_since(now) / 2
}

/// Whether a candidate's reply means it cannot serve the read: no reply at all, or a reply
/// that says so. A replica that reports any unavailable error gives the read to the next
/// copy. The primary that moved answers for its repository, so for it only a reply saying
/// the node is not serving does; its other errors are the answer to the read.
pub fn cannot_serve(reply: Option<&Reply>, moved_primary: bool) -> bool {
    match reply {
        None => true,
        Some(reply) => match &reply.result {
            Err(Failure::Repository(error)) if moved_primary => error.is_storage_node_unavailable(),
            Err(Failure::Repository(error)) => {
                matches!(error.kind, ErrorKind::Unavailable | ErrorKind::NotFound)
            }
            _ => false,
        },
    }
}

/// One warning for each read that a copy answered because the primary did not reply.
pub fn log_failover(repository: Uuid, primary: i64, copy: i64) {
    tracing::warn!(
        repository = %repository,
        primary,
        copy,
        "primary unreachable; read served by a copy"
    );
}
