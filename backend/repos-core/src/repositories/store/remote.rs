//! Forward repository operations to their owning storage node.
use crate::context::NodeContext;
use crate::repositories::store::local::LocalStore;
use crate::repositories::store::RepoStore;
use crate::store_types::BlobBytes;
use crate::store_types::BlobMetadata;
use crate::store_types::WorkflowPayloadEntry;
use crate::store_types::MAX_BLOB_RESPONSE_BYTES;
use crate::store_types::MAX_WORKFLOW_PAYLOAD_BYTES;
use crate::wire::Failure;
use crate::wire::Operation;
use crate::wire::Request;
use codeyard_git::error::RepoError;
use codeyard_git::BlobContent;
use codeyard_git::CiWorkflowSource;
use codeyard_git::MAX_CI_WORKFLOW_BYTES;
use codeyard_git::MAX_WORKFLOW_DIRECTORY_ENTRIES;
use codeyard_git::MAX_WORKFLOW_FILE_NAME_BYTES;
use codeyard_storage::range::ByteRange as BlobRange;
use serde::de::DeserializeOwned;
use std::path::Path;

fn decode_json<T: DeserializeOwned>(
    value: serde_json::Value,
    body: &[u8],
    raw_json: bool,
) -> Result<T, RepoError> {
    if raw_json && !body.is_empty() {
        serde_json::from_slice(body).map_err(|_| crate::wire::unavailable())
    } else {
        // Peers predating raw JSON responses still return the value in the header.
        serde_json::from_value(value).map_err(|_| crate::wire::unavailable())
    }
}

#[derive(Clone, Debug)]
pub(super) struct RemoteStore {
    pub(super) disk_path: String,
    pub(super) node_id: i64,
    context: Option<NodeContext>,
    ci_run: Option<crate::wire::CiRunAccess>,
}

impl RemoteStore {
    pub(super) fn unavailable() -> Self {
        Self::new("", 0, None)
    }

    pub(super) fn new(disk_path: &str, node_id: i64, context: Option<&NodeContext>) -> Self {
        Self {
            disk_path: disk_path.to_owned(),
            node_id,
            context: context.cloned(),
            ci_run: None,
        }
    }

    pub(super) fn for_ci_run(&mut self, id: i64, worker: &str) {
        self.ci_run = Some(crate::wire::CiRunAccess {
            id,
            worker: worker.to_owned(),
        });
    }

    async fn exchange_with_actor(
        &self,
        actor: Option<i64>,
        operation: Operation,
        body: &[u8],
    ) -> Result<(crate::wire::Reply, Vec<u8>), RepoError> {
        self.exchange_with_actor_and_limit(actor, operation, body, crate::wire::MAX_BYTES)
            .await
    }

    async fn exchange_with_actor_and_limit(
        &self,
        actor: Option<i64>,
        operation: Operation,
        body: &[u8],
        response_body_limit: usize,
    ) -> Result<(crate::wire::Reply, Vec<u8>), RepoError> {
        let context = self.context.as_ref().ok_or_else(crate::wire::unavailable)?;
        context
            .call_with_response_limit(
                self.node_id,
                Request {
                    repository: crate::wire::repository_uuid(&self.disk_path)?,
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
    ) -> Result<(crate::wire::Reply, Vec<u8>), RepoError> {
        self.exchange_with_actor(None, operation, body).await
    }

    pub(super) async fn call<T: DeserializeOwned>(
        &self,
        operation: Operation,
        body: &[u8],
    ) -> Result<T, RepoError> {
        let raw_json = matches!(
            &operation,
            Operation::FilePaths { .. }
                | Operation::GitTree { .. }
                | Operation::IntroducedBlobs { .. }
                | Operation::GitTag { .. }
                | Operation::TemplateTexts { .. }
                | Operation::CommitsBetween { .. }
                | Operation::Blame { .. }
                | Operation::Commits { .. }
                | Operation::CommitPage { .. }
                | Operation::PathCommits { .. }
                | Operation::ComparisonFrom { .. }
                | Operation::SearchCode { raw_json: true, .. }
                | Operation::CommitStatistics { raw_json: true, .. }
        );
        let (reply, body) = self.exchange(operation, body).await?;
        let value = reply.result.map_err(|failure| match failure {
            Failure::Repository(error) => error,
            Failure::Merge(_) => crate::wire::invalid("unexpected merge failure"),
        })?;
        decode_json(value, &body, raw_json)
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
            Failure::Merge(_) => crate::wire::invalid("unexpected merge failure"),
        })?;
        serde_json::from_value(value).map_err(|_| crate::wire::unavailable())
    }

    pub(super) async fn bytes_as(
        &self,
        actor: i64,
        operation: Operation,
    ) -> Result<Vec<u8>, RepoError> {
        let (reply, body) = self
            .exchange_with_actor_and_limit(
                Some(actor),
                operation,
                b"",
                codeyard_support::object_limits::MAX_OBJECT_BYTES as usize,
            )
            .await?;
        match reply.result {
            Ok(_) => Ok(body),
            Err(Failure::Repository(error)) => Err(error),
            Err(Failure::Merge(_)) => Err(crate::wire::invalid("unexpected merge failure")),
        }
    }

    pub(super) async fn bytes(
        &self,
        operation: Operation,
        body: &[u8],
    ) -> Result<Vec<u8>, RepoError> {
        self.bytes_with_limit(operation, body, crate::wire::MAX_BYTES)
            .await
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
            Err(Failure::Merge(_)) => Err(crate::wire::invalid("unexpected merge failure")),
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
            Failure::Merge(_) => crate::wire::invalid("unexpected merge failure"),
        })?;
        let manifest: Vec<WorkflowPayloadEntry> =
            serde_json::from_value(value).map_err(|_| crate::wire::unavailable())?;
        if manifest.len() > MAX_WORKFLOW_DIRECTORY_ENTRIES {
            return Err(crate::wire::unavailable());
        }

        let mut offset = 0usize;
        let mut files = Vec::with_capacity(manifest.len());
        for entry in manifest {
            if entry.path.len() > ".github/workflows/".len() + MAX_WORKFLOW_FILE_NAME_BYTES
                || entry.length > MAX_CI_WORKFLOW_BYTES
            {
                return Err(crate::wire::unavailable());
            }
            let end = offset
                .checked_add(entry.length)
                .filter(|end| *end <= body.len())
                .ok_or_else(crate::wire::unavailable)?;
            let content = std::str::from_utf8(&body[offset..end])
                .map_err(|_| crate::wire::unavailable())?
                .to_owned();
            files.push(CiWorkflowSource {
                path: entry.path,
                content,
            });
            offset = end;
        }
        if offset != body.len() {
            return Err(crate::wire::unavailable());
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
                    serde_json::from_value(value).map_err(|_| crate::wire::unavailable())?;
                blob.content = String::from_utf8(body).map_err(|_| crate::wire::unavailable())?;
                Ok(blob)
            }
            Err(Failure::Repository(error)) => Err(error),
            Err(Failure::Merge(_)) => Err(crate::wire::unavailable()),
        }
    }

    pub(super) async fn blob_range(
        &self,
        git_ref: &str,
        path: &str,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        self.blob_part(Operation::BlobBytes {
            git_ref: git_ref.into(),
            path: path.into(),
            range,
        })
        .await
    }

    pub(super) async fn blob_by_id_range(
        &self,
        id: &str,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        self.blob_part(Operation::BlobByIdRange {
            id: id.into(),
            range,
        })
        .await
    }

    /// Blob bytes travel as the raw reply body beside their JSON metadata.
    async fn blob_part(&self, operation: Operation) -> Result<BlobBytes, RepoError> {
        let (reply, bytes) = self
            .exchange_with_actor_and_limit(None, operation, b"", MAX_BLOB_RESPONSE_BYTES)
            .await?;
        match reply.result {
            Ok(value) => {
                let metadata: BlobMetadata =
                    serde_json::from_value(value).map_err(|_| crate::wire::unavailable())?;
                Ok(BlobBytes {
                    id: metadata.id,
                    bytes,
                    is_binary: metadata.is_binary,
                    size: metadata.size,
                    offset: metadata.offset,
                    mode: metadata.mode,
                    preview: metadata.preview,
                })
            }
            Err(Failure::Repository(error)) => Err(error),
            Err(Failure::Merge(_)) => Err(crate::wire::unavailable()),
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
                    crate::repositories::store::Backend::Remote(remote)
                        if remote.node_id == self.node_id => {}
                    _ => {
                        return Err(crate::wire::invalid(
                            "fork source is not on the destination node",
                        ))
                    }
                }
                Some(crate::wire::repository_uuid(store.location())?)
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
        session: crate::store_types::GitSession,
        streams: crate::store_types::GitStreams<'_>,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let context = self.context.as_ref().ok_or_else(crate::wire::unavailable)?;
        context
            .git_session(
                self.node_id,
                Request {
                    repository: crate::wire::repository_uuid(&self.disk_path)?,
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
        let oid = git2::Oid::from_str(sha)
            .map_err(|_| crate::wire::invalid("invalid checkout commit"))?;
        // Network admission is separate from execution on this node or its peer.
        let permit = codeyard_git::repository::process::forwarding_permit()
            .await
            .map_err(RepoError::from)?;
        let mut request = Vec::new();
        codeyard_repos::source::append_packet(&mut request, &format!("want {oid}\n"));
        request.extend_from_slice(b"00000009done\n");
        let parent = destination
            .parent()
            .ok_or_else(|| crate::wire::invalid("invalid checkout directory"))?;
        let pack = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        let mut output = tokio::fs::File::from_std(
            pack.reopen()
                .map_err(|error| RepoError::from(anyhow::Error::from(error)))?,
        );
        let status = self
            .git_session(
                crate::store_types::GitSession::new(
                    crate::store_types::GitService::UploadPack,
                    true,
                ),
                crate::store_types::GitStreams {
                    input: &mut request.as_slice(),
                    output: &mut output,
                    errors: &mut tokio::io::sink(),
                },
            )
            .await
            .map_err(RepoError::from)?;
        if !status.success() {
            return Err(crate::wire::unavailable());
        }
        use tokio::io::AsyncWriteExt;
        output
            .flush()
            .await
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        drop(output);
        let lock = codeyard_git::locks::lock(destination).await;
        LocalStore::checkout_pack(destination, sha, &self.disk_path, pack, permit, lock).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_replies_decode_from_legacy_headers_or_negotiated_raw_frames() {
        let expected = vec!["path/one".to_owned()];
        let legacy: Vec<String> = decode_json(serde_json::json!(expected), b"", true).unwrap();
        let raw: Vec<String> =
            decode_json(serde_json::Value::Null, br#"["path/one"]"#, true).unwrap();
        assert_eq!(legacy, expected);
        assert_eq!(raw, expected);
        assert!(decode_json::<Vec<String>>(serde_json::Value::Null, b"broken", true).is_err());
    }
}
