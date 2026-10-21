//! The values that cross the store boundary: Git sessions, blob reads, merge requests and
//! the size limits that apply to them.
use crate::backend::db::models::Repository;
use crate::backend::git::{MAX_CI_WORKFLOW_BYTES, MAX_WORKFLOW_DIRECTORY_ENTRIES};
use serde::{Deserialize, Serialize};

/// The two Git server programs a session can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitService {
    UploadPack,
    ReceivePack,
}

impl GitService {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "upload-pack",
            Self::ReceivePack => "receive-pack",
        }
    }
}

/// A live Git service, interactive over SSH and stateless over HTTP.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitSession {
    pub service: GitService,
    pub stateless: bool,
    pub git_protocol: Option<String>,
    pub credential: Option<crate::backend::repositories::git_sessions::ReceiveCredential>,
    pub owner: String,
    /// Set only after a storage-side CI claim check; never accepted from the wire.
    #[serde(skip)]
    pub allow_unreachable: bool,
}

impl GitSession {
    pub fn new(service: GitService, stateless: bool) -> Self {
        Self {
            service,
            stateless,
            git_protocol: None,
            credential: None,
            owner: String::new(),
            allow_unreachable: false,
        }
    }
}

pub struct GitStreams<'a> {
    pub input: &'a mut (dyn tokio::io::AsyncRead + Unpin + Send),
    pub output: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    pub errors: &'a mut (dyn tokio::io::AsyncWrite + Unpin + Send),
}

/// A bounded part of an immutable blob, with metadata for range and preview
/// responses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobBytes {
    pub id: String,
    pub bytes: Vec<u8>,
    pub is_binary: bool,
    pub size: u64,
    pub offset: u64,
    /// The Git file mode of the entry, such as 0o100644 or 0o120000.
    pub mode: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobRange {
    #[default]
    Full,
    /// A preview window. An offset beyond EOF produces an empty window at EOF.
    Window { offset: u64, length: u64 },
    /// An HTTP byte range with an inclusive optional end.
    Bytes { start: u64, end: Option<u64> },
    /// The final `length` bytes.
    Suffix { length: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BlobMetadata {
    pub id: String,
    pub is_binary: bool,
    pub size: u64,
    pub offset: u64,
    pub mode: u32,
}

pub const MAX_BLOB_RESPONSE_BYTES: usize = crate::backend::git::MAX_BLOB_MATERIALIZE_BYTES;
pub const MAX_ARCHIVE_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_COMMIT_PAGE_SIZE: usize = 100;
pub const MAX_COMMIT_OFFSET: usize = 100_000;
pub const MAX_CONTRIBUTOR_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) const MAX_WORKFLOW_PAYLOAD_BYTES: usize =
    MAX_WORKFLOW_DIRECTORY_ENTRIES * MAX_CI_WORKFLOW_BYTES;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkflowPayloadEntry {
    pub path: String,
    pub length: usize,
}

/// How the commits of a pull request reach the base branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    /// A merge commit with the base and the head as parents.
    #[default]
    Merge,
    /// One commit on the base branch that holds the whole change.
    Squash,
    /// Each commit of the head, replayed in order on the base branch.
    Rebase,
}

impl MergeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }

    /// The method a merge form or request names, if it is one.
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Merge, Self::Squash, Self::Rebase]
            .into_iter()
            .find(|method| method.as_str() == name)
    }
}

/// The merge methods that `repository` allows, in the order the merge form offers them.
pub fn allowed_merge_methods(repository: &Repository) -> Vec<MergeMethod> {
    [
        (repository.allow_merge_commit, MergeMethod::Merge),
        (repository.allow_squash_merge, MergeMethod::Squash),
        (repository.allow_rebase_merge, MergeMethod::Rebase),
    ]
    .into_iter()
    .filter_map(|(allowed, method)| allowed.then_some(method))
    .collect()
}

/// Merge `head_branch` into `base_branch`, as approved when the branches were at
/// `head` and `base`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    pub head_branch: String,
    pub base_branch: String,
    pub head: String,
    pub base: String,
    /// The message of a merge or squash commit. A rebase keeps each commit's own message.
    pub message: String,
    /// The committer: the person who merges.
    pub name: String,
    pub email: String,
    pub method: MergeMethod,
    /// The author of a squash commit. Without one, the committer is the author.
    pub author_name: Option<String>,
    pub author_email: Option<String>,
}

/// One branch that a restack moves: the commit it must still be at, and the commit it moves to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestackMove {
    pub branch: String,
    pub old: String,
    pub new: String,
}

/// Why a prepared merge changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeFailure {
    /// The branches conflict, or Git could not build the merge commit.
    CannotMerge,
    /// A branch moved after the merge commit was built, so nothing was published.
    BranchMoved,
    /// The owning storage node could not be reached.
    Unavailable,
}
