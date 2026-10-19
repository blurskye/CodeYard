use crate::backend::repositories::store::{
    BlobRange, CommitFilter, ErrorKind, FileWrite, GitService, GitSession, MergeFailure,
    MergeMethod, MergeRequest, PreparedFile, RepoError, RestackMove, UploadWrite, MAX_EDITOR_BYTES,
    MAX_UPLOAD_BYTES,
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub const MAX_JSON: usize = 8 * 1024 * 1024;
pub const MAX_BYTES: usize = 128 * 1024 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(300);

/// A first page leaves its zero offset out, so an older node answers it as before.
fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Operation {
    Initialize {
        source: Option<Uuid>,
        default_branch: String,
    },
    Remove,
    /// Fetches a commit of another repository on this node into a pull ref.
    FetchPullHead {
        source: Uuid,
        number: i32,
        head: String,
        expected_head: Option<String>,
    },
    Exists,
    SizeBytes,
    Refs,
    /// The same refs as `Refs`, answered only by the primary. For a caller that writes the
    /// answer into the database.
    PrimaryRefs,
    Info,
    CommitCount,
    Tree {
        git_ref: String,
        path: String,
    },
    WorkflowFiles {
        revision: String,
    },
    Blob {
        git_ref: String,
        path: String,
    },
    BlobBytes {
        git_ref: String,
        path: String,
        #[serde(default)]
        range: BlobRange,
    },
    BlobById {
        id: String,
    },
    Commits {
        git_ref: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "CommitFilter::is_empty")]
        filter: CommitFilter,
    },
    CommitPage {
        git_ref: String,
        offset: usize,
        limit: usize,
        #[serde(default, skip_serializing_if = "CommitFilter::is_empty")]
        filter: CommitFilter,
    },
    /// Lines of the files at `git_ref` that contain `text`, as `RepoStore::search_code` finds them.
    SearchCode {
        git_ref: String,
        text: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: usize,
        limit: usize,
    },
    /// Commits that changed `path`, as `git log -- <path>` lists them.
    PathCommits {
        git_ref: String,
        path: String,
        offset: usize,
        limit: usize,
        #[serde(default, skip_serializing_if = "CommitFilter::is_empty")]
        filter: CommitFilter,
    },
    Commit {
        revision: String,
    },
    /// The same commit as `Commit`, answered only by the primary. For a caller that writes
    /// the answer into the database.
    PrimaryCommit {
        revision: String,
    },
    CommitDiff {
        revision: String,
    },
    Diff {
        base_ref: String,
        head_ref: String,
    },
    /// Commits reachable from `head_ref` but not from `base_ref`, newest first.
    CommitsBetween {
        base_ref: String,
        head_ref: String,
        limit: usize,
    },
    DiffFile {
        base_ref: String,
        head_ref: String,
        path: String,
    },
    ChangedPaths {
        base_ref: String,
        head_ref: String,
    },
    DiffFrom {
        source: Uuid,
        base_ref: String,
        head_ref: String,
    },
    Tags,
    Archive {
        git_ref: String,
        prefix: String,
        format: crate::backend::git::ArchiveFormat,
    },
    Languages {
        git_ref: String,
    },
    FilePaths {
        git_ref: String,
        limit: usize,
    },
    Blame {
        git_ref: String,
        path: String,
    },
    Contributors,
    IsAncestor {
        ancestor: String,
        descendant: String,
    },
    /// Each head's divergence from `base` and the time of its commit, for the branch list.
    BranchStatuses {
        base: String,
        heads: Vec<String>,
    },
    ResolveCiCommit {
        reference: String,
        sha: Option<String>,
        must_be_on_ref: bool,
    },
    CiWorkflowFile {
        sha: String,
        workflow_path: String,
    },
    CreateBranch {
        name: String,
        sha: String,
    },
    DeleteBranch {
        name: String,
    },
    SetDefaultBranch {
        name: String,
    },
    CreateTag {
        name: String,
        sha: String,
        message: Option<String>,
        tagger_name: String,
        tagger_email: String,
    },
    DeleteTag {
        name: String,
    },
    PrepareFile {
        write: FileWrite,
        author: String,
        email: String,
    },
    PublishFile {
        prepared: PreparedFile,
    },
    SaveFile {
        write: FileWrite,
        author: String,
        email: String,
    },
    PrepareUpload {
        write: UploadWrite,
        author: String,
        email: String,
    },
    Merge {
        request: MergeRequest,
    },
    PrepareMerge {
        request: MergeRequest,
    },
    PublishMerge {
        head_branch: String,
        base_branch: String,
        head: String,
        base: String,
        result: String,
        method: MergeMethod,
    },
    /// Publishes a stacked merge commit. Each head is `(head branch or pull ref, commit)`.
    PublishStackedMerge {
        heads: Vec<(String, String)>,
        base_branch: String,
        base: String,
        result: String,
    },
    /// Builds the commits of a restack: `from..head` replayed onto `onto`.
    PrepareRestack {
        from: String,
        head: String,
        onto: String,
        name: String,
        email: String,
    },
    /// Moves the branches of a restack, each checked against its old commit, with `base_branch`
    /// checked against `base`.
    PublishRestack {
        base_branch: String,
        base: String,
        moves: Vec<RestackMove>,
    },
    AdvertiseRefs {
        service: GitService,
    },
    GitSession(GitSession),
    /// A replica copy asks its primary for the objects it lacks. `have` lists the tips the
    /// copy already holds. Streamed like a Git session: the reply lists the primary's HEAD
    /// and refs, then a pack of the objects those refs need follows.
    ReplicaPack {
        have: Vec<String>,
    },
}

impl Operation {
    /// Whether a copy may answer this operation while the primary cannot be reached. These
    /// are the reads that pages, the API and Git clients make on one repository. Writes,
    /// receive-pack, replica transfers and repository setup are never read-only. CI admission
    /// reads, the merge check, the existence, size and changed-paths checks, and comparisons
    /// between two repositories stay on the primary: the primary acts on their answers, no
    /// page shows them, or both repositories would have to be copied to one node. A ref or
    /// commit read whose answer is written to the database stays on the primary too, so a
    /// lagging copy cannot move a recorded revision back. There is no wildcard arm, so a new
    /// operation needs a decision here.
    pub fn is_read_only(&self) -> bool {
        match self {
            Self::Tree { .. }
            | Self::Blob { .. }
            | Self::BlobBytes { .. }
            | Self::BlobById { .. }
            | Self::Commits { .. }
            | Self::CommitPage { .. }
            | Self::SearchCode { .. }
            | Self::PathCommits { .. }
            | Self::Commit { .. }
            | Self::CommitDiff { .. }
            | Self::Diff { .. }
            | Self::CommitsBetween { .. }
            | Self::DiffFile { .. }
            | Self::Refs
            | Self::Info
            | Self::CommitCount
            | Self::Tags
            | Self::Archive { .. }
            | Self::Languages { .. }
            | Self::FilePaths { .. }
            | Self::Blame { .. }
            | Self::Contributors
            | Self::BranchStatuses { .. } => true,
            Self::AdvertiseRefs { service } => *service == GitService::UploadPack,
            Self::GitSession(session) => session.service == GitService::UploadPack,
            Self::Exists
            | Self::SizeBytes
            | Self::WorkflowFiles { .. }
            | Self::PrimaryRefs
            | Self::PrimaryCommit { .. }
            | Self::ChangedPaths { .. }
            | Self::DiffFrom { .. }
            | Self::IsAncestor { .. }
            | Self::ResolveCiCommit { .. }
            | Self::CiWorkflowFile { .. } => false,
            Self::Initialize { .. }
            | Self::Remove
            | Self::FetchPullHead { .. }
            | Self::PrepareFile { .. }
            | Self::PrepareUpload { .. }
            | Self::PublishFile { .. }
            | Self::SaveFile { .. }
            | Self::Merge { .. }
            | Self::PrepareMerge { .. }
            | Self::PublishMerge { .. }
            | Self::PublishStackedMerge { .. }
            | Self::PrepareRestack { .. }
            | Self::PublishRestack { .. }
            | Self::CreateBranch { .. }
            | Self::DeleteBranch { .. }
            | Self::SetDefaultBranch { .. }
            | Self::CreateTag { .. }
            | Self::DeleteTag { .. }
            | Self::ReplicaPack { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiRunAccess {
    pub id: i64,
    pub worker: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub repository: Uuid,
    pub operation: Operation,
    /// The user whose current authority is required for ref publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<i64>,
    /// Present only on repository reads made for one claimed CI run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_run: Option<CiRunAccess>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Failure {
    Repository(RepoError),
    Merge(MergeFailure),
}

impl From<RepoError> for Failure {
    fn from(error: RepoError) -> Self {
        Self::Repository(error)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub result: Result<Value, Failure>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct GitExit {
    pub(super) result: Result<i32, RepoError>,
    pub(super) stderr: String,
}

pub(crate) fn invalid(message: &str) -> RepoError {
    RepoError::new(ErrorKind::Invalid, message)
}

pub fn unavailable() -> RepoError {
    RepoError::storage_node_unavailable()
}

/// The request-body limit for an operation. All other buffered operations are bodyless.
pub(super) fn operation_body_limit(operation: &Operation) -> usize {
    match operation {
        Operation::PrepareFile { .. } | Operation::SaveFile { .. } => MAX_EDITOR_BYTES,
        Operation::PrepareUpload { .. } => MAX_UPLOAD_BYTES,
        _ => 0,
    }
}

/// Check the length before allocating, including zero-length binary bodies.
pub async fn read_frame(reader: &mut (impl AsyncRead + Unpin), limit: usize) -> Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    ensure!(length <= limit, "RPC frame exceeds its limit");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    limit: usize,
) -> Result<()> {
    ensure!(bytes.len() <= limit, "RPC frame exceeds its limit");
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    // A TLS writer can accept the bytes while records still wait behind a full socket. The
    // peer may need this frame before it sends anything, so push it out now.
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_preserves_typed_reason_and_released_wire_shape() {
        let error = unavailable();
        assert!(error.is_storage_node_unavailable());
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "storage node unavailable");
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "Unavailable",
                "message": "storage node unavailable"
            })
        );
        let decoded: RepoError = serde_json::from_value(value).unwrap();
        assert!(decoded.is_storage_node_unavailable());
        assert!(RepoError::from(anyhow::Error::new(error)).is_storage_node_unavailable());
    }

    #[test]
    fn request_keeps_existing_repository_operation_serialization() {
        let repository = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let value = serde_json::to_value(Request {
            repository,
            operation: Operation::Remove,
            actor: None,
            ci_run: None,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "repository": "00000000-0000-0000-0000-000000000001",
                "operation": "Remove"
            })
        );
        let request: Request = serde_json::from_value(value).unwrap();
        assert!(request.actor.is_none());
        assert!(matches!(request.operation, Operation::Remove));
    }

    #[test]
    fn commit_paging_keeps_the_released_commits_request_unchanged() {
        let operation: Operation = serde_json::from_value(serde_json::json!({
            "Commits": {"git_ref": "main", "limit": 30}
        }))
        .unwrap();
        assert!(matches!(
            operation,
            Operation::Commits {
                git_ref,
                limit: 30,
                ..
            } if git_ref == "main"
        ));
        assert_eq!(
            serde_json::to_value(Operation::Commits {
                git_ref: "main".into(),
                limit: 30,
                filter: Default::default(),
            })
            .unwrap(),
            serde_json::json!({"Commits": {"git_ref": "main", "limit": 30}})
        );
        assert_eq!(
            serde_json::to_value(Operation::CommitPage {
                git_ref: "main".into(),
                offset: 30,
                limit: 30,
                filter: Default::default(),
            })
            .unwrap(),
            serde_json::json!({
                "CommitPage": {"git_ref": "main", "offset": 30, "limit": 30}
            })
        );
    }

    #[test]
    fn code_search_is_a_read_that_a_replica_copy_may_answer() {
        let operation = Operation::SearchCode {
            git_ref: "main".into(),
            text: "needle".into(),
            offset: 0,
            limit: 5,
        };
        assert_eq!(
            serde_json::to_value(&operation).unwrap(),
            serde_json::json!({"SearchCode": {"git_ref": "main", "text": "needle", "limit": 5}})
        );
        assert!(operation.is_read_only());
    }

    #[test]
    fn old_shaped_mutation_operation_deserializes_without_an_embedded_actor() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "repository": "00000000-0000-0000-0000-000000000001",
            "operation": {
                "CreateBranch": {
                    "name": "feature",
                    "sha": "1111111111111111111111111111111111111111"
                }
            }
        }))
        .unwrap();
        assert!(request.actor.is_none());
        assert!(matches!(
            request.operation,
            Operation::CreateBranch { name, sha }
                if name == "feature" && sha == "1111111111111111111111111111111111111111"
        ));
    }

    #[test]
    fn only_editor_preparation_and_save_accept_request_bytes() {
        let write = FileWrite {
            branch: "main".into(),
            path: "file.txt".into(),
            message: "Save file".into(),
            expected_revision: "unborn".into(),
            creating: true,
            deleting: false,
        };
        assert_eq!(
            operation_body_limit(&Operation::PrepareFile {
                write: write.clone(),
                author: "Author".into(),
                email: "author@example.invalid".into(),
            }),
            MAX_EDITOR_BYTES
        );
        assert_eq!(
            operation_body_limit(&Operation::SaveFile {
                write,
                author: "Author".into(),
                email: "author@example.invalid".into(),
            }),
            MAX_EDITOR_BYTES
        );
        assert_eq!(operation_body_limit(&Operation::Refs), 0);
        assert_eq!(operation_body_limit(&Operation::PrimaryRefs), 0);
        assert_eq!(
            operation_body_limit(&Operation::WorkflowFiles {
                revision: "main".into()
            }),
            0
        );
    }

    #[test]
    fn primary_only_reads_are_not_read_only() {
        assert!(Operation::Refs.is_read_only());
        assert!(Operation::Commit {
            revision: "main".into()
        }
        .is_read_only());
        assert!(!Operation::PrimaryRefs.is_read_only());
        assert!(!Operation::PrimaryCommit {
            revision: "main".into()
        }
        .is_read_only());
    }

    #[tokio::test]
    async fn oversized_frame_is_refused_before_reading_its_body() {
        let (mut writer, mut reader) = tokio::io::duplex(4);
        writer.write_u32((MAX_JSON + 1) as u32).await.unwrap();
        // The writer remains open without sending a body. Reading the prefix
        // must refuse immediately, rather than wait or allocate the claimed size.
        let result =
            tokio::time::timeout(Duration::from_secs(1), read_frame(&mut reader, MAX_JSON))
                .await
                .unwrap();
        assert!(result.is_err());
    }

    /// A TLS connection between two node keys, carried by an in-memory pipe of `capacity`
    /// bytes, so that writes meet a full pipe as they do on a busy socket.
    async fn tls_pair(
        capacity: usize,
    ) -> (
        tokio_rustls::client::TlsStream<tokio::io::DuplexStream>,
        tokio_rustls::server::TlsStream<tokio::io::DuplexStream>,
    ) {
        use crate::backend::cluster::{tls, transport::ALPN_RPC, NodeKey};
        let server_key = NodeKey::generate();
        let client_key = NodeKey::generate();
        let (client_io, server_io) = tokio::io::duplex(capacity);
        let acceptor = tokio_rustls::TlsAcceptor::from(tls::server_config(&server_key).unwrap());
        let connector = tokio_rustls::TlsConnector::from(
            tls::client_config(&client_key, server_key.public_key(), ALPN_RPC).unwrap(),
        );
        let name = tokio_rustls::rustls::pki_types::ServerName::IpAddress(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST).into(),
        );
        let (client, server) = tokio::join!(
            connector.connect(name, client_io),
            acceptor.accept(server_io)
        );
        (client.unwrap(), server.unwrap())
    }

    #[tokio::test]
    async fn a_frame_reaches_the_peer_before_write_frame_returns_over_tls() {
        let (mut client, mut server) = tls_pair(1024).await;
        let payload = vec![7_u8; 128 * 1024];
        let sent = payload.clone();
        let reader = tokio::spawn(async move { read_frame(&mut server, MAX_BYTES).await });
        write_frame(&mut client, &payload, MAX_BYTES).await.unwrap();
        // Nothing else is written after this, so the frame must already be on its way.
        let received = tokio::time::timeout(Duration::from_secs(10), reader)
            .await
            .expect("the frame never arrived")
            .unwrap()
            .unwrap();
        assert!(received == sent, "the frame arrived damaged");
    }

    #[tokio::test]
    async fn a_forwarded_chunk_reaches_the_peer_while_its_input_stays_open() {
        let (client, mut server) = tls_pair(1024).await;
        let (mut input, forwarded) = tokio::io::duplex(256 * 1024);
        let (_, writer) = tokio::io::split(client);
        let (activity, _receiver) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let copying = tokio::spawn(crate::backend::git::repository::process::copy(
            forwarded, writer, activity, false,
        ));
        let payload = vec![9_u8; 64 * 1024];
        input.write_all(&payload).await.unwrap();
        // The input stays open, as it does while Git waits for the peer's answer.
        let mut received = vec![0_u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(10), server.read_exact(&mut received))
            .await
            .expect("the chunk never arrived")
            .unwrap();
        assert!(received == payload, "the chunk arrived damaged");
        drop(input);
        copying.await.unwrap().unwrap();
    }
}
