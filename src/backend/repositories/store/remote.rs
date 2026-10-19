//! Forward repository operations to their owning storage node.
use super::{
    local::LocalStore, BlobBytes, BlobContent, BlobMetadata, BlobRange, CiWorkflowSource,
    RepoError, RepoStore, WorkflowPayloadEntry, MAX_BLOB_RESPONSE_BYTES, MAX_CI_WORKFLOW_BYTES,
    MAX_WORKFLOW_DIRECTORY_ENTRIES, MAX_WORKFLOW_FILE_NAME_BYTES, MAX_WORKFLOW_PAYLOAD_BYTES,
};
use crate::backend::cluster::{
    rpc::{self, Failure, Operation, Request},
    NodeContext,
};
use serde::de::DeserializeOwned;
use std::path::Path;

#[derive(Clone, Debug)]
pub(super) struct RemoteStore {
    pub(super) disk_path: String,
    pub(super) node_id: i64,
    context: Option<NodeContext>,
    ci_run: Option<rpc::CiRunAccess>,
}

impl RemoteStore {
    pub(super) fn new(disk_path: &str, node_id: i64, context: Option<&NodeContext>) -> Self {
        Self {
            disk_path: disk_path.to_owned(),
            node_id,
            context: context.cloned(),
            ci_run: None,
        }
    }

    pub(super) fn for_ci_run(&mut self, id: i64, worker: &str) {
        self.ci_run = Some(rpc::CiRunAccess {
            id,
            worker: worker.to_owned(),
        });
    }

    async fn exchange_with_actor(
        &self,
        actor: Option<i64>,
        operation: Operation,
        body: &[u8],
    ) -> Result<(rpc::Reply, Vec<u8>), RepoError> {
        self.exchange_with_actor_and_limit(actor, operation, body, rpc::MAX_BYTES)
            .await
    }

    async fn exchange_with_actor_and_limit(
        &self,
        actor: Option<i64>,
        operation: Operation,
        body: &[u8],
        response_body_limit: usize,
    ) -> Result<(rpc::Reply, Vec<u8>), RepoError> {
        let context = self.context.as_ref().ok_or_else(rpc::unavailable)?;
        context
            .call_with_response_limit(
                self.node_id,
                Request {
                    repository: rpc::repository_uuid(&self.disk_path)?,
                    operation,
                    actor,
                    ci_run: self.ci_run.clone(),
                },
                body,
                response_body_limit,
            )
            .await
    }

    async fn exchange(
        &self,
        operation: Operation,
        body: &[u8],
    ) -> Result<(rpc::Reply, Vec<u8>), RepoError> {
        self.exchange_with_actor(None, operation, body).await
    }

    pub(super) async fn call<T: DeserializeOwned>(
        &self,
        operation: Operation,
        body: &[u8],
    ) -> Result<T, RepoError> {
        let (reply, _) = self.exchange(operation, body).await?;
        let value = reply.result.map_err(|failure| match failure {
            Failure::Repository(error) => error,
            Failure::Merge(_) => rpc::wire::invalid("unexpected merge failure"),
        })?;
        serde_json::from_value(value).map_err(|_| rpc::unavailable())
    }

    pub(super) async fn call_as<T: DeserializeOwned>(
        &self,
        actor: i64,
        operation: Operation,
        body: &[u8],
    ) -> Result<T, RepoError> {
        let (reply, _) = self
            .exchange_with_actor(Some(actor), operation, body)
            .await?;
        let value = reply.result.map_err(|failure| match failure {
            Failure::Repository(error) => error,
            Failure::Merge(_) => rpc::wire::invalid("unexpected merge failure"),
        })?;
        serde_json::from_value(value).map_err(|_| rpc::unavailable())
    }

    pub(super) async fn bytes(
        &self,
        operation: Operation,
        body: &[u8],
    ) -> Result<Vec<u8>, RepoError> {
        self.bytes_with_limit(operation, body, rpc::MAX_BYTES).await
    }

    pub(super) async fn bytes_with_limit(
        &self,
        operation: Operation,
        body: &[u8],
        response_body_limit: usize,
    ) -> Result<Vec<u8>, RepoError> {
        let (reply, body) = self
            .exchange_with_actor_and_limit(None, operation, body, response_body_limit)
            .await?;
        match reply.result {
            Ok(_) => Ok(body),
            Err(Failure::Repository(error)) => Err(error),
            Err(Failure::Merge(_)) => Err(rpc::wire::invalid("unexpected merge failure")),
        }
    }

    pub(super) async fn workflow_files(
        &self,
        revision: &str,
    ) -> Result<Vec<CiWorkflowSource>, RepoError> {
        let (reply, body) = self
            .exchange_with_actor_and_limit(
                None,
                Operation::WorkflowFiles {
                    revision: revision.into(),
                },
                b"",
                MAX_WORKFLOW_PAYLOAD_BYTES,
            )
            .await?;
        let value = reply.result.map_err(|failure| match failure {
            Failure::Repository(error) => error,
            Failure::Merge(_) => rpc::wire::invalid("unexpected merge failure"),
        })?;
        let manifest: Vec<WorkflowPayloadEntry> =
            serde_json::from_value(value).map_err(|_| rpc::unavailable())?;
        if manifest.len() > MAX_WORKFLOW_DIRECTORY_ENTRIES {
            return Err(rpc::unavailable());
        }

        let mut offset = 0usize;
        let mut files = Vec::with_capacity(manifest.len());
        for entry in manifest {
            if entry.path.len() > ".github/workflows/".len() + MAX_WORKFLOW_FILE_NAME_BYTES
                || entry.length > MAX_CI_WORKFLOW_BYTES
            {
                return Err(rpc::unavailable());
            }
            let end = offset
                .checked_add(entry.length)
                .filter(|end| *end <= body.len())
                .ok_or_else(rpc::unavailable)?;
            let content = std::str::from_utf8(&body[offset..end])
                .map_err(|_| rpc::unavailable())?
                .to_owned();
            files.push(CiWorkflowSource {
                path: entry.path,
                content,
            });
            offset = end;
        }
        if offset != body.len() {
            return Err(rpc::unavailable());
        }
        Ok(files)
    }

    pub(super) async fn blob(&self, git_ref: &str, path: &str) -> Result<BlobContent, RepoError> {
        let (reply, body) = self
            .exchange_with_actor_and_limit(
                None,
                Operation::Blob {
                    git_ref: git_ref.into(),
                    path: path.into(),
                },
                b"",
                MAX_BLOB_RESPONSE_BYTES,
            )
            .await?;
        match reply.result {
            Ok(value) => {
                let mut blob: BlobContent =
                    serde_json::from_value(value).map_err(|_| rpc::unavailable())?;
                blob.content = String::from_utf8(body).map_err(|_| rpc::unavailable())?;
                Ok(blob)
            }
            Err(Failure::Repository(error)) => Err(error),
            Err(Failure::Merge(_)) => Err(rpc::unavailable()),
        }
    }

    pub(super) async fn blob_range(
        &self,
        git_ref: &str,
        path: &str,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        let (reply, bytes) = self
            .exchange_with_actor_and_limit(
                None,
                Operation::BlobBytes {
                    git_ref: git_ref.into(),
                    path: path.into(),
                    range,
                },
                b"",
                MAX_BLOB_RESPONSE_BYTES,
            )
            .await?;
        match reply.result {
            Ok(value) => {
                let metadata: BlobMetadata =
                    serde_json::from_value(value).map_err(|_| rpc::unavailable())?;
                Ok(BlobBytes {
                    id: metadata.id,
                    bytes,
                    is_binary: metadata.is_binary,
                    size: metadata.size,
                    offset: metadata.offset,
                })
            }
            Err(Failure::Repository(error)) => Err(error),
            Err(Failure::Merge(_)) => Err(rpc::unavailable()),
        }
    }

    pub(super) async fn initialize(
        &self,
        source: Option<&RepoStore>,
        default_branch: &str,
    ) -> Result<(), RepoError> {
        let source = match source {
            Some(store) => {
                match &store.0 {
                    super::Backend::Remote(remote) if remote.node_id == self.node_id => {}
                    _ => {
                        return Err(rpc::wire::invalid(
                            "fork source is not on the destination node",
                        ))
                    }
                }
                Some(rpc::repository_uuid(store.location())?)
            }
            None => None,
        };
        self.call(
            Operation::Initialize {
                source,
                default_branch: default_branch.into(),
            },
            b"",
        )
        .await
    }

    pub(super) async fn git_session(
        &self,
        session: super::GitSession,
        streams: super::GitStreams<'_>,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let context = self.context.as_ref().ok_or_else(rpc::unavailable)?;
        context
            .git_session(
                self.node_id,
                Request {
                    repository: rpc::repository_uuid(&self.disk_path)?,
                    operation: Operation::GitSession(session),
                    actor: None,
                    ci_run: self.ci_run.clone(),
                },
                streams,
            )
            .await
            .map(|(_, status)| status)
    }

    pub(super) async fn checkout(&self, destination: &Path, sha: &str) -> Result<(), RepoError> {
        let oid =
            git2::Oid::from_str(sha).map_err(|_| rpc::wire::invalid("invalid checkout commit"))?;
        // A checkout is a session like any other, also when this node serves it from its own
        // copy, which takes no permit of its own.
        let _permit = crate::backend::git::repository::process::session_permit()
            .await
            .map_err(RepoError::from)?;
        let want = format!("want {oid}\n");
        let request = format!("{:04x}{want}00000009done\n", want.len() + 4);
        let pack = tempfile::NamedTempFile::new()
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        let mut output = tokio::fs::File::from_std(
            pack.reopen()
                .map_err(|error| RepoError::from(anyhow::Error::from(error)))?,
        );
        let status = self
            .git_session(
                super::GitSession::new(super::GitService::UploadPack, true),
                super::GitStreams {
                    input: &mut request.as_bytes(),
                    output: &mut output,
                    errors: &mut tokio::io::sink(),
                },
            )
            .await
            .map_err(RepoError::from)?;
        if !status.success() {
            return Err(rpc::unavailable());
        }
        use tokio::io::AsyncWriteExt;
        output
            .flush()
            .await
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        drop(output);
        LocalStore::checkout_pack(destination, sha, pack).await
    }
}
