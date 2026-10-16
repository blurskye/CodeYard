//! The one way to reach a repository's Git data.
//!
//! Callers get a [`RepoStore`] from [`repo_store`] and ask it for what they need.
//! Nothing outside this module opens a repository directory or runs `git` against
//! one. Local stores use this node's disk; remote stores forward the same calls
//! over iroh to the owning storage node. Inputs and outputs are owned values.
//! The one exception is [`RepoStore::local_path_for_spawning_git`], used by the
//! host OpenSSH gateway.
mod editor;
mod local;
mod remote;

use crate::backend::repositories::protection::{self, MERGE_COMMIT_BLOCKED};
use crate::backend::{
    cluster::{
        rpc::{self, Operation},
        NodeContext,
    },
    db::{self, models::Repository},
    git,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt, path::Path};

pub(crate) use crate::backend::git::validate_merge_revision;
pub use crate::backend::git::{
    BlameHunk, BlobContent, CiCommit, CiWorkflowFile, CiWorkflowSource, CommitInfo,
    ContributorInfo, DiffFileInfo, DiffInfo, FileBlame, FileList, LanguageStat, RepoInfo, TagInfo,
    TreeEntry, MAX_CI_CHANGED_PATHS, MAX_CI_CHANGED_PATH_BYTES, MAX_CI_CHANGED_PATH_TOTAL_BYTES,
    MAX_CI_WORKFLOW_BYTES, MAX_CONTRIBUTORS, MAX_DIFF_FILES, MAX_DIFF_FILE_PATCH_BYTES,
    MAX_DIFF_TOTAL_PATCH_BYTES, MAX_LANGUAGE_FILES, MAX_REPOSITORY_REFS, MAX_TREE_ENTRIES,
    MAX_WORKFLOW_DIRECTORY_ENTRIES, MAX_WORKFLOW_FILE_NAME_BYTES,
};
pub use editor::{
    FileWrite, PreparedFile, UploadFile, UploadWrite, MAX_EDITOR_BYTES, MAX_UPLOAD_BYTES,
    MAX_UPLOAD_FILES, MAX_UPLOAD_FILE_BYTES,
};
pub(crate) use local::{ensure_ref_room, record_ref_change};

/// The store for a repository row. Cheap to create and to clone.
pub fn repo_store(repo: &Repository, context: Option<&NodeContext>) -> RepoStore {
    match repo.storage_node_id {
        None => RepoStore::from_disk_path(&repo.disk_path),
        Some(id) if context.is_some_and(|context| context.node_id == id) => {
            let context = context.expect("this node owns the repository");
            match rpc::repository_uuid(&repo.disk_path) {
                Ok(uuid) => RepoStore::on_node(
                    &context
                        .repos_path
                        .join(format!("{uuid}.git"))
                        .to_string_lossy(),
                    id,
                ),
                Err(_) => RepoStore(Backend::Remote(remote::RemoteStore::new(
                    &repo.disk_path,
                    id,
                    None,
                ))),
            }
        }
        Some(id) => RepoStore(Backend::Remote(remote::RemoteStore::new(
            &repo.disk_path,
            id,
            context,
        ))),
    }
}

impl RepoStore {
    /// CI workers receive a storage node and UUID, not a repository database row.
    pub fn remote(uuid: uuid::Uuid, node_id: i64, context: &NodeContext) -> Self {
        Self(Backend::Remote(remote::RemoteStore::new(
            &format!("{uuid}.git"),
            node_id,
            Some(context),
        )))
    }

    pub(crate) fn for_ci_run(mut self, id: i64, worker: &str) -> Self {
        if let Backend::Remote(remote) = &mut self.0 {
            remote.for_ci_run(id, worker);
        }
        self
    }
}

/// Why a store call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    /// The branch, tag, commit or object does not exist, or the revision is malformed.
    NotFound,
    /// The commit exists but the path inside it does not.
    MissingPath,
    /// The request is not acceptable as written.
    Invalid,
    /// A ref moved since the caller last looked.
    Conflict,
    /// Git or the disk failed, or the repository cannot be reached.
    Unavailable,
}

/// Refused when the storage node finds the merge method turned off in the repository.
pub const MERGE_METHOD_NOT_ALLOWED: &str = "This merge method is not allowed in this repository.";

/// Specific failure reasons remain internal; the released RPC shape is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoErrorReason {
    StorageNodeUnavailable,
    SessionsBusy,
    WriteDenied,
    ProtectedBranch,
    MergeCommitBlocked,
    AuthorityUnavailable,
    AppliedFollowupFailed,
    ResponseTooLarge,
    RangeNotSatisfiable,
    BranchMissing,
    MergeMethodNotAllowed,
}

/// A failed store call, with stable wire fields and an internal typed reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepoError {
    pub kind: ErrorKind,
    #[serde(skip)]
    pub reason: Option<RepoErrorReason>,
    /// What Git said, or a fixed sentence for requests refused here.
    pub message: String,
}

impl RepoError {
    pub fn is_storage_node_unavailable(&self) -> bool {
        self.reason == Some(RepoErrorReason::StorageNodeUnavailable)
    }

    pub fn is_write_denied(&self) -> bool {
        self.reason == Some(RepoErrorReason::WriteDenied)
    }

    pub fn is_protected_branch(&self) -> bool {
        self.reason == Some(RepoErrorReason::ProtectedBranch)
    }

    pub fn is_merge_commit_blocked(&self) -> bool {
        self.reason == Some(RepoErrorReason::MergeCommitBlocked)
    }

    pub fn is_authority_unavailable(&self) -> bool {
        self.reason == Some(RepoErrorReason::AuthorityUnavailable)
    }

    pub fn is_applied_followup_failed(&self) -> bool {
        self.reason == Some(RepoErrorReason::AppliedFollowupFailed)
    }

    pub fn is_response_too_large(&self) -> bool {
        self.reason == Some(RepoErrorReason::ResponseTooLarge)
    }

    pub fn is_range_not_satisfiable(&self) -> bool {
        self.reason == Some(RepoErrorReason::RangeNotSatisfiable)
    }

    pub fn is_branch_missing(&self) -> bool {
        self.reason == Some(RepoErrorReason::BranchMissing)
    }

    pub fn is_merge_method_not_allowed(&self) -> bool {
        self.reason == Some(RepoErrorReason::MergeMethodNotAllowed)
    }

    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            reason: None,
            message: message.into(),
        }
    }

    pub fn storage_node_unavailable() -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            reason: Some(RepoErrorReason::StorageNodeUnavailable),
            message: "storage node unavailable".into(),
        }
    }

    pub fn sessions_busy() -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            reason: Some(RepoErrorReason::SessionsBusy),
            message: "Git sessions busy".into(),
        }
    }

    pub fn write_denied() -> Self {
        Self {
            kind: ErrorKind::Invalid,
            reason: Some(RepoErrorReason::WriteDenied),
            message: "repository write denied".into(),
        }
    }

    pub fn protected_branch() -> Self {
        Self {
            kind: ErrorKind::Invalid,
            reason: Some(RepoErrorReason::ProtectedBranch),
            message: "protected branch".into(),
        }
    }

    /// A branch protection rule refused the write. `message` is the rule's own text, which
    /// survives the cluster wire (see `protection::refusal_text`).
    pub fn protected_refusal(message: &'static str) -> Self {
        Self {
            kind: ErrorKind::Invalid,
            reason: Some(RepoErrorReason::ProtectedBranch),
            message: message.into(),
        }
    }

    /// The merge would leave merge commits on a branch that requires a linear history.
    pub fn merge_commit_blocked() -> Self {
        Self {
            kind: ErrorKind::Invalid,
            reason: Some(RepoErrorReason::MergeCommitBlocked),
            message: MERGE_COMMIT_BLOCKED.into(),
        }
    }

    pub fn authority_unavailable() -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            reason: Some(RepoErrorReason::AuthorityUnavailable),
            message: "Repository authority is unavailable".into(),
        }
    }

    pub fn applied_followup_failed() -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            reason: Some(RepoErrorReason::AppliedFollowupFailed),
            message: "Git changed, but default branch synchronization failed".into(),
        }
    }

    pub fn response_too_large() -> Self {
        Self {
            kind: ErrorKind::Invalid,
            reason: Some(RepoErrorReason::ResponseTooLarge),
            message: "repository response exceeds its limit".into(),
        }
    }

    pub fn range_not_satisfiable() -> Self {
        Self {
            kind: ErrorKind::Invalid,
            reason: Some(RepoErrorReason::RangeNotSatisfiable),
            message: "repository byte range is not satisfiable".into(),
        }
    }

    /// The storage node found the merge method turned off in the repository.
    pub fn merge_method_not_allowed() -> Self {
        Self {
            kind: ErrorKind::Invalid,
            reason: Some(RepoErrorReason::MergeMethodNotAllowed),
            message: MERGE_METHOD_NOT_ALLOWED.into(),
        }
    }

    /// A file write named a branch the primary does not have.
    pub fn branch_missing() -> Self {
        Self {
            kind: ErrorKind::Conflict,
            reason: Some(RepoErrorReason::BranchMissing),
            message: "The branch does not exist.".into(),
        }
    }
}

impl<'de> Deserialize<'de> for RepoError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireError {
            kind: ErrorKind,
            message: String,
        }
        let wire = WireError::deserialize(deserializer)?;
        // Released peers carry only kind/message. Decode fixed typed reasons
        // here; operation and HTTP callers never select behavior from error text.
        Ok(match (wire.kind, wire.message.as_str()) {
            (ErrorKind::Unavailable, "storage node unavailable") => {
                Self::storage_node_unavailable()
            }
            (ErrorKind::Unavailable, "Git sessions busy") => Self::sessions_busy(),
            (ErrorKind::Invalid, "repository write denied") => Self::write_denied(),
            (ErrorKind::Invalid, "protected branch") => Self::protected_branch(),
            (ErrorKind::Invalid, MERGE_COMMIT_BLOCKED) => Self::merge_commit_blocked(),
            (ErrorKind::Unavailable, "Repository authority is unavailable") => {
                Self::authority_unavailable()
            }
            (ErrorKind::Unavailable, "Git changed, but default branch synchronization failed") => {
                Self::applied_followup_failed()
            }
            (ErrorKind::Invalid, "repository response exceeds its limit") => {
                Self::response_too_large()
            }
            (ErrorKind::Invalid, "repository byte range is not satisfiable") => {
                Self::range_not_satisfiable()
            }
            (ErrorKind::Conflict, "The branch does not exist.") => Self::branch_missing(),
            (ErrorKind::Invalid, MERGE_METHOD_NOT_ALLOWED) => Self::merge_method_not_allowed(),
            _ => match protection::refusal_text(&wire.message) {
                Some(text) => Self::protected_refusal(text),
                None => Self::new(wire.kind, wire.message),
            },
        })
    }
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RepoError {}

fn invalid_ref(kind: &str) -> RepoError {
    RepoError::new(ErrorKind::Invalid, format!("Invalid {kind} name"))
}

impl From<anyhow::Error> for RepoError {
    /// Sorts a Git failure by its libgit2 code, so callers can tell a missing
    /// branch or path from an outage without knowing about libgit2.
    fn from(error: anyhow::Error) -> Self {
        if let Some(error) = error.downcast_ref::<RepoError>() {
            return error.clone();
        }
        if error.is::<crate::backend::git::repository::process::SessionsBusy>() {
            return Self::sessions_busy();
        }
        if error.is::<crate::backend::git::repository::PullHeadConflict>() {
            return Self::new(ErrorKind::Conflict, "Git pull request ref changed");
        }
        if let Some(conflict) =
            error.downcast_ref::<crate::backend::git::repository::MergeConflict>()
        {
            return Self::new(ErrorKind::Conflict, conflict.to_string());
        }
        if let Some(unsupported) =
            error.downcast_ref::<crate::backend::git::repository::UnsupportedMerge>()
        {
            return Self::new(ErrorKind::Invalid, unsupported.to_string());
        }
        if error.is::<crate::backend::git::repository::process::OutputLimitExceeded>()
            || error.is::<crate::backend::git::ObjectTooLarge>()
        {
            return Self::response_too_large();
        }
        if error.is::<crate::backend::git::InvalidByteRange>() {
            return Self::range_not_satisfiable();
        }
        if let Some(refused) = error.downcast_ref::<crate::backend::git::repository::BlameRefused>()
        {
            return Self::new(ErrorKind::Invalid, refused.0);
        }
        let kind = match error.downcast_ref::<git2::Error>() {
            Some(error)
                if error.code() == git2::ErrorCode::NotFound
                    && error.class() == git2::ErrorClass::Tree =>
            {
                ErrorKind::MissingPath
            }
            // A repository directory that cannot be opened is an outage, not a
            // missing branch.
            Some(error)
                if error.code() == git2::ErrorCode::NotFound
                    && matches!(
                        error.class(),
                        git2::ErrorClass::Os | git2::ErrorClass::Repository
                    ) =>
            {
                ErrorKind::Unavailable
            }
            Some(error)
                if matches!(
                    error.code(),
                    git2::ErrorCode::NotFound | git2::ErrorCode::InvalidSpec
                ) =>
            {
                ErrorKind::NotFound
            }
            _ => ErrorKind::Unavailable,
        };
        Self::new(kind, error.to_string())
    }
}

/// The two Git server programs a session can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitService {
    UploadPack,
    ReceivePack,
}

impl GitService {
    fn name(self) -> &'static str {
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
}

impl GitSession {
    pub fn new(service: GitService, stateless: bool) -> Self {
        Self {
            service,
            stateless,
            git_protocol: None,
            credential: None,
            owner: String::new(),
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
}

pub const MAX_BLOB_RESPONSE_BYTES: usize = crate::backend::git::MAX_BLOB_MATERIALIZE_BYTES;
pub const MAX_ARCHIVE_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_COMMIT_PAGE_SIZE: usize = 100;
pub const MAX_COMMIT_OFFSET: usize = 100_000;
pub const MAX_COMMIT_METADATA_BYTES: usize = crate::backend::git::MAX_COMMIT_OBJECT_BYTES;
pub const MAX_COMMIT_PAGE_BYTES: usize = crate::backend::git::MAX_COMMIT_PAGE_BYTES;
pub const MAX_CONTRIBUTOR_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_TAG_METADATA_BYTES: usize = crate::backend::git::MAX_TAG_OBJECT_BYTES;
pub const MAX_TAG_RESPONSE_BYTES: usize = crate::backend::git::MAX_TAG_RESPONSE_BYTES;

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

/// A repository's Git data, wherever it lives.
#[derive(Clone, Debug)]
pub struct RepoStore(Backend);

#[derive(Clone, Debug)]
enum Backend {
    Local(local::LocalStore),
    Remote(remote::RemoteStore),
}

impl RepoStore {
    fn local(&self) -> Result<&local::LocalStore, RepoError> {
        match &self.0 {
            Backend::Local(local) => Ok(local),
            Backend::Remote(_) => Err(rpc::wire::invalid(
                "fork source is not on the destination node",
            )),
        }
    }

    // ----- Opening and lifecycle -----

    /// The store for a stored `disk_path` value, for when no `Repository` is at
    /// hand: removing storage after its row is gone, tools and tests. Application
    /// code uses [`repo_store`].
    pub fn from_disk_path(disk_path: &str) -> RepoStore {
        RepoStore(Backend::Local(local::LocalStore::new(disk_path, None)))
    }

    /// A repository directory on storage node `node_id`. Its writes stop once the
    /// repository has been promoted to another node.
    pub(crate) fn on_node(disk_path: &str, node_id: i64) -> RepoStore {
        RepoStore(Backend::Local(local::LocalStore::new(
            disk_path,
            Some(node_id),
        )))
    }

    /// Creates an empty bare repository `<repos_root>/<uuid>.git`, or a fork of
    /// `source` (its branches and tags, nothing else). Nothing is left behind on
    /// failure. Repositories never move once created, so renames and owner changes
    /// don't touch the disk.
    pub async fn create(
        repos_root: &Path,
        default_branch: &str,
        source: Option<&RepoStore>,
    ) -> Result<RepoStore, RepoError> {
        let local = local::LocalStore::create(
            repos_root,
            default_branch,
            source.map(RepoStore::local).transpose()?,
        )
        .await?;
        Ok(RepoStore(Backend::Local(local)))
    }

    /// Initializes a bare repository at this store's location, which has to be new
    /// or empty. [`RepoStore::create`] picks a fresh location and calls this.
    pub async fn initialize(
        &self,
        source: Option<&RepoStore>,
        default_branch: &str,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .initialize(source.map(RepoStore::local).transpose()?, default_branch)
                    .await
            }
            Backend::Remote(remote) => remote.initialize(source, default_branch).await,
        }
    }

    /// Copies a fork privately, then checks live source access before publishing it.
    pub async fn initialize_fork(
        &self,
        pool: &db::DbPool,
        source_repo_id: i64,
        actor: i64,
        source: &RepoStore,
        default_branch: &str,
    ) -> Result<(), RepoError> {
        match (&self.0, &source.0) {
            (Backend::Local(local), Backend::Local(from)) => {
                local
                    .initialize_fork(pool, source_repo_id, actor, from, default_branch)
                    .await
            }
            (Backend::Remote(remote), Backend::Remote(from)) if remote.node_id == from.node_id => {
                remote
                    .call_as(
                        actor,
                        Operation::Initialize {
                            source: Some(rpc::repository_uuid(&from.disk_path)?),
                            default_branch: default_branch.into(),
                        },
                        b"",
                    )
                    .await
            }
            _ => Err(rpc::wire::invalid(
                "fork source is not on the destination node",
            )),
        }
    }

    /// What the database keeps in `repositories.disk_path` to find this repository
    /// again. Only database code and operator tools have a use for it.
    pub fn location(&self) -> &str {
        match &self.0 {
            Backend::Local(local) => local.disk_path(),
            Backend::Remote(remote) => &remote.disk_path,
        }
    }

    /// Deletes the repository's storage. Storage that is already gone counts as
    /// removed. Any other failure is logged with the path and returned, after a
    /// few tries: by the time a repository is deleted its database row is gone, so
    /// the files cannot be found again from the database, and the caller must not
    /// tell the user the repository is gone while its data is still on disk.
    pub async fn remove(&self) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.remove().await,
            Backend::Remote(remote) => remote.call(Operation::Remove, b"").await,
        }
    }

    /// Whether the storage holds a Git repository at all.
    pub async fn exists(&self) -> bool {
        match &self.0 {
            Backend::Local(local) => local.exists().await,
            Backend::Remote(remote) => remote.call(Operation::Exists, b"").await.unwrap_or(false),
        }
    }

    /// Bytes the repository takes on disk.
    pub async fn size_bytes(&self) -> Result<u64, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.size_bytes().await,
            Backend::Remote(remote) => remote.call(Operation::SizeBytes, b"").await,
        }
    }

    // ----- Reads -----

    /// Every branch and tag as `refs/heads/<name>` or `refs/tags/<name>` mapped to
    /// the object it points at.
    pub async fn refs(&self) -> Result<HashMap<String, String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.refs().await,
            Backend::Remote(remote) => remote.call(Operation::Refs, b"").await,
        }
    }

    /// The same refs as [`RepoStore::refs`], but a remote store never falls back to a
    /// replica copy, which may lag behind. For callers that write the answer into the
    /// database.
    pub async fn refs_from_primary(&self) -> Result<HashMap<String, String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.refs().await,
            Backend::Remote(remote) => remote.call(Operation::PrimaryRefs, b"").await,
        }
    }

    /// Every ref the repository stores, including pull request heads, for a replica. Only a
    /// repository on this node has them.
    pub async fn mirror_refs(&self) -> Result<HashMap<String, String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.mirror_refs().await,
            Backend::Remote(_) => Err(rpc::wire::invalid("replica refs are listed on their node")),
        }
    }

    /// Branch names and the default branch, without traversing history.
    pub async fn info(&self) -> Result<RepoInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.info().await,
            Backend::Remote(remote) => remote.call(Operation::Info, b"").await,
        }
    }

    /// Count commits reachable from all refs. Only statistics callers need this walk.
    pub async fn commit_count(&self) -> Result<u64, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.commit_count().await,
            Backend::Remote(remote) => remote.call(Operation::CommitCount, b"").await,
        }
    }

    /// The entries of the directory `path` ("" is the root) at `git_ref`, folders
    /// first, each with the commit that last touched it.
    pub async fn tree(&self, git_ref: &str, path: &str) -> Result<Vec<TreeEntry>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.tree(git_ref, path).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::Tree {
                            git_ref: git_ref.into(),
                            path: path.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Every file path at `git_ref`, sorted. The listing keeps at most `limit` paths.
    pub async fn file_paths(&self, git_ref: &str, limit: usize) -> Result<FileList, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.file_paths(git_ref, limit).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::FilePaths {
                            git_ref: git_ref.into(),
                            limit,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Direct workflow files at a revision, bounded by directory, name and blob size.
    pub async fn workflow_files(&self, revision: &str) -> Result<Vec<CiWorkflowSource>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.workflow_files(revision).await,
            Backend::Remote(remote) => remote.workflow_files(revision).await,
        }
    }

    /// A file as bounded text (binary files come back as a short placeholder).
    pub async fn blob(&self, git_ref: &str, path: &str) -> Result<BlobContent, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.blob(git_ref, path).await,
            Backend::Remote(remote) => remote.blob(git_ref, path).await,
        }
    }

    /// A bounded part of a file's exact bytes.
    pub async fn blob_range(
        &self,
        git_ref: &str,
        path: &str,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.blob_range(git_ref, path, range).await,
            Backend::Remote(remote) => remote.blob_range(git_ref, path, range).await,
        }
    }

    /// A complete file when it fits the repository response limit.
    pub async fn blob_bytes(&self, git_ref: &str, path: &str) -> Result<BlobBytes, RepoError> {
        self.blob_range(git_ref, path, BlobRange::Full).await
    }

    /// A blob by its object ID, wherever it appears in the repository.
    pub async fn blob_by_id(&self, id: &str) -> Result<Vec<u8>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.blob_by_id(id).await,
            Backend::Remote(remote) => {
                remote
                    .bytes_with_limit(
                        Operation::BlobById { id: id.into() },
                        b"",
                        MAX_BLOB_RESPONSE_BYTES,
                    )
                    .await
            }
        }
    }

    /// Up to `limit` commits after `offset`, starting at `git_ref` and visiting the
    /// newest pending ancestor first. Unvisited history is never loaded.
    pub async fn commits(
        &self,
        git_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        if offset > MAX_COMMIT_OFFSET || limit > MAX_COMMIT_PAGE_SIZE {
            return Err(RepoError::response_too_large());
        }
        match &self.0 {
            Backend::Local(local) => local.commits(git_ref, offset, limit).await,
            Backend::Remote(remote) => {
                let operation = if offset == 0 {
                    Operation::Commits {
                        git_ref: git_ref.into(),
                        limit,
                    }
                } else {
                    Operation::CommitPage {
                        git_ref: git_ref.into(),
                        offset,
                        limit,
                    }
                };
                remote.call(operation, b"").await
            }
        }
    }

    /// One commit, by any revision Git understands.
    pub async fn commit(&self, revision: &str) -> Result<CommitInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.commit(revision).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::Commit {
                            revision: revision.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// The same commit as [`RepoStore::commit`], but a remote store never falls back to a
    /// replica copy, which may lag behind. For callers that write the answer into the
    /// database.
    pub async fn commit_from_primary(&self, revision: &str) -> Result<CommitInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.commit(revision).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrimaryCommit {
                            revision: revision.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// What one commit changed relative to its first parent.
    pub async fn commit_diff(&self, revision: &str) -> Result<DiffInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.commit_diff(revision).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::CommitDiff {
                            revision: revision.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// What `head_ref` changed since it split from `base_ref` (a three-dot diff).
    pub async fn diff(&self, base_ref: &str, head_ref: &str) -> Result<DiffInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.diff(base_ref, head_ref).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::Diff {
                            base_ref: base_ref.into(),
                            head_ref: head_ref.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// One file of a three-dot comparison. Only that file's patch is read, so the size of other
    /// files does not matter.
    pub async fn diff_file(
        &self,
        base_ref: &str,
        head_ref: &str,
        path: &str,
    ) -> Result<DiffInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.diff_file(base_ref, head_ref, path).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::DiffFile {
                            base_ref: base_ref.into(),
                            head_ref: head_ref.into(),
                            path: path.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Changed path names for a three-dot comparison, with count and byte bounds.
    pub async fn changed_paths(
        &self,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<Vec<String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.changed_paths(base_ref, head_ref).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::ChangedPaths {
                            base_ref: base_ref.into(),
                            head_ref: head_ref.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// A three-dot comparison with the head in another repository, without importing objects.
    /// Both repositories must be stored on the same node.
    pub async fn diff_from(
        &self,
        source: &RepoStore,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<DiffInfo, RepoError> {
        match (&self.0, &source.0) {
            (Backend::Local(base), Backend::Local(from)) => {
                base.diff_from(from, base_ref, head_ref).await
            }
            (Backend::Remote(base), Backend::Remote(from)) if base.node_id == from.node_id => {
                base.call(
                    Operation::DiffFrom {
                        source: rpc::repository_uuid(&from.disk_path)?,
                        base_ref: base_ref.into(),
                        head_ref: head_ref.into(),
                    },
                    b"",
                )
                .await
            }
            _ => Err(rpc::wire::invalid(
                "comparison repositories are not on the same node",
            )),
        }
    }

    /// Tags with the message of annotated ones.
    pub async fn tags(&self) -> Result<Vec<TagInfo>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.tags().await,
            Backend::Remote(remote) => remote.call(Operation::Tags, b"").await,
        }
    }

    /// The tree at `git_ref` as a zip file whose entries sit under `prefix/`.
    pub async fn archive_zip(&self, git_ref: &str, prefix: &str) -> Result<Vec<u8>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.archive_zip(git_ref, prefix).await,
            Backend::Remote(remote) => {
                remote
                    .bytes_with_limit(
                        Operation::ArchiveZip {
                            git_ref: git_ref.into(),
                            prefix: prefix.into(),
                        },
                        b"",
                        MAX_ARCHIVE_RESPONSE_BYTES,
                    )
                    .await
            }
        }
    }

    /// Bytes per language at `git_ref`, from file extensions.
    pub async fn languages(&self, git_ref: &str) -> Result<Vec<LanguageStat>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.languages(git_ref).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::Languages {
                            git_ref: git_ref.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// A text file as of `git_ref`, with the commit that last changed each run of its lines.
    pub async fn blame(&self, git_ref: &str, path: &str) -> Result<FileBlame, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.blame(git_ref, path).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::Blame {
                            git_ref: git_ref.into(),
                            path: path.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Authors across all branches with their commit counts.
    pub async fn contributors(&self) -> Result<Vec<ContributorInfo>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.contributors().await,
            Backend::Remote(remote) => remote.call(Operation::Contributors, b"").await,
        }
    }

    /// Whether `ancestor` is reachable from `descendant`. Both are full commit IDs.
    pub async fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.is_ancestor(ancestor, descendant).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::IsAncestor {
                            ancestor: ancestor.into(),
                            descendant: descendant.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    // ----- CI -----

    /// Picks the commit a CI run will use: `sha` if given, otherwise the tip of
    /// `reference` (a branch, a tag or a full ref name). With `must_be_on_ref` the
    /// `sha` has to be reachable from the ref's tip. Failures carry the sentences
    /// CI admission reports to its callers.
    pub async fn resolve_ci_commit(
        &self,
        reference: &str,
        sha: Option<&str>,
        must_be_on_ref: bool,
    ) -> Result<CiCommit, RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .resolve_ci_commit(reference, sha, must_be_on_ref)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::ResolveCiCommit {
                            reference: reference.into(),
                            sha: sha.map(str::to_owned),
                            must_be_on_ref,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// One workflow file at an exact commit, if it is a regular tracked text file of
    /// reasonable size. Failures carry the sentences CI admission reports.
    pub async fn ci_workflow_file(
        &self,
        sha: &str,
        workflow_path: &str,
    ) -> Result<CiWorkflowFile, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.ci_workflow_file(sha, workflow_path).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::CiWorkflowFile {
                            sha: sha.into(),
                            workflow_path: workflow_path.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Clones the repository into `destination` on this machine, which must not
    /// exist yet, and checks out `sha` there. The clone shares nothing with the
    /// source. A CI worker runs its job in it.
    pub async fn checkout(&self, destination: &Path, sha: &str) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.checkout(destination, sha).await,
            Backend::Remote(remote) => remote.checkout(destination, sha).await,
        }
    }

    // ----- Writes -----

    /// Fetches a fork commit and keeps it under `refs/pull/<number>/head`.
    /// Both repositories must be on the same machine: a fork is stored next to
    /// its source. Publication compares the pull ref with `expected_head`; a
    /// missing ref can be repaired.
    #[expect(
        clippy::too_many_arguments,
        reason = "Pull-head publication carries explicit source, actor, revision and CAS inputs."
    )]
    pub async fn fetch_pull_head(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        source_repo_id: i64,
        actor: i64,
        source: &RepoStore,
        number: i32,
        head: &str,
        expected_head: Option<&str>,
    ) -> Result<(), RepoError> {
        match (&self.0, &source.0) {
            (Backend::Local(base), Backend::Local(from)) => {
                base.fetch_pull_head(
                    pool,
                    repo_id,
                    source_repo_id,
                    actor,
                    from,
                    number,
                    head,
                    expected_head,
                )
                .await
            }
            (Backend::Remote(base), Backend::Remote(from)) if base.node_id == from.node_id => {
                base.call_as(
                    actor,
                    Operation::FetchPullHead {
                        source: rpc::repository_uuid(&from.disk_path)?,
                        number,
                        head: head.into(),
                        expected_head: expected_head.map(str::to_owned),
                    },
                    b"",
                )
                .await
            }
            _ => Err(rpc::wire::invalid(
                "fork source is not on the destination node",
            )),
        }
    }

    /// Creates a branch at the commit `sha`; returns that commit's ID.
    pub async fn create_branch_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        name: &str,
        sha: &str,
    ) -> Result<String, RepoError> {
        if git::branch_ref(name).is_none() {
            return Err(invalid_ref("branch"));
        }
        match &self.0 {
            Backend::Local(local) => local.create_branch(pool, repo_id, actor, name, sha).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::CreateBranch {
                            name: name.into(),
                            sha: sha.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Deletes a branch. The default branch cannot be deleted.
    pub async fn delete_branch_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        name: &str,
    ) -> Result<(), RepoError> {
        if git::branch_ref(name).is_none() {
            return Err(invalid_ref("branch"));
        }
        match &self.0 {
            Backend::Local(local) => local.delete_branch(pool, repo_id, actor, name).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(actor, Operation::DeleteBranch { name: name.into() }, b"")
                    .await
            }
        }
    }

    /// Creates a tag at the commit `sha`: annotated when there is a message,
    /// lightweight otherwise.
    #[expect(
        clippy::too_many_arguments,
        reason = "Tag publication has explicit authority and identity inputs."
    )]
    pub async fn create_tag_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        name: &str,
        sha: &str,
        message: Option<&str>,
        tagger_name: &str,
        tagger_email: &str,
    ) -> Result<TagInfo, RepoError> {
        if git::tag_ref(name).is_none() {
            return Err(invalid_ref("tag"));
        }
        match &self.0 {
            Backend::Local(local) => {
                local
                    .create_tag(
                        pool,
                        repo_id,
                        actor,
                        name,
                        sha,
                        message,
                        tagger_name,
                        tagger_email,
                    )
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::CreateTag {
                            name: name.into(),
                            sha: sha.into(),
                            message: message.map(str::to_owned),
                            tagger_name: tagger_name.into(),
                            tagger_email: tagger_email.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Deletes a tag.
    pub async fn delete_tag_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        name: &str,
    ) -> Result<(), RepoError> {
        if git::tag_ref(name).is_none() {
            return Err(invalid_ref("tag"));
        }
        match &self.0 {
            Backend::Local(local) => local.delete_tag(pool, repo_id, actor, name).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(actor, Operation::DeleteTag { name: name.into() }, b"")
                    .await
            }
        }
    }

    /// Creates the blob, trees and commit without changing a ref.
    pub async fn prepare_file(
        &self,
        write: &FileWrite,
        content: &[u8],
        author: &str,
        email: &str,
    ) -> Result<PreparedFile, RepoError> {
        write.validate()?;
        match &self.0 {
            Backend::Local(local) => local.prepare_file(write, content, author, email).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrepareFile {
                            write: write.clone(),
                            author: author.into(),
                            email: email.into(),
                        },
                        content,
                    )
                    .await
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "File save has explicit authority and commit identity inputs."
    )]
    pub async fn save_file_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        write: &FileWrite,
        content: &[u8],
        author: &str,
        email: &str,
    ) -> Result<CommitInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                let prepared = local.prepare_file(write, content, author, email).await?;
                local.publish_file(pool, repo_id, actor, &prepared).await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::SaveFile {
                            write: write.clone(),
                            author: author.into(),
                            email: email.into(),
                        },
                        content,
                    )
                    .await
            }
        }
    }

    /// Creates the one commit of an upload without changing a ref. The files' bytes follow
    /// each other in `content`, in the order of `write.files`.
    pub async fn prepare_upload(
        &self,
        write: &UploadWrite,
        content: &[u8],
        author: &str,
        email: &str,
    ) -> Result<PreparedFile, RepoError> {
        write.validate()?;
        match &self.0 {
            Backend::Local(local) => local.prepare_upload(write, content, author, email).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrepareUpload {
                            write: write.clone(),
                            author: author.into(),
                            email: email.into(),
                        },
                        content,
                    )
                    .await
            }
        }
    }

    /// Moves the branch to a prepared file commit after rechecking current authority.
    pub async fn publish_file_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        prepared: &PreparedFile,
    ) -> Result<CommitInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.publish_file(pool, repo_id, actor, prepared).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishFile {
                            prepared: prepared.clone(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Builds the commit that `request` asks for without touching any branch. A merge
    /// commit is always two-parent, even when `head` already contains `base`. Conflicts and
    /// unsupported histories are errors.
    pub async fn prepare_merge(&self, request: &MergeRequest) -> Result<String, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.prepare_merge(request).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrepareMerge {
                            request: request.clone(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Publishes a prepared merge by moving `base_branch` to `result`,
    /// failing if either branch is no longer at `base` or `head`.
    #[expect(
        clippy::too_many_arguments,
        reason = "Merge publication carries explicit authority and CAS inputs."
    )]
    pub async fn publish_merge_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        head_branch: &str,
        base_branch: &str,
        head: &str,
        base: &str,
        result: &str,
        method: MergeMethod,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .publish_merge(
                        pool,
                        repo_id,
                        actor,
                        &[(head_branch, head)],
                        base_branch,
                        base,
                        result,
                        method,
                    )
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishMerge {
                            head_branch: head_branch.into(),
                            base_branch: base_branch.into(),
                            head: head.into(),
                            base: base.into(),
                            result: result.into(),
                            method,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Publishes a stacked merge commit: moves `base_branch` from `base` to `result` in one ref
    /// transaction, provided every head in `heads` is still at its prepared commit. Each head is
    /// `(head branch or pull ref, commit)`.
    #[expect(
        clippy::too_many_arguments,
        reason = "Merge publication carries explicit authority and CAS inputs."
    )]
    pub async fn publish_stacked_merge_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        heads: &[(String, String)],
        base_branch: &str,
        base: &str,
        result: &str,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                let heads: Vec<(&str, &str)> = heads
                    .iter()
                    .map(|(branch, head)| (branch.as_str(), head.as_str()))
                    .collect();
                local
                    .publish_merge(
                        pool,
                        repo_id,
                        actor,
                        &heads,
                        base_branch,
                        base,
                        result,
                        MergeMethod::Merge,
                    )
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishStackedMerge {
                            heads: heads.to_vec(),
                            base_branch: base_branch.into(),
                            base: base.into(),
                            result: result.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Builds the commits that replay `from..head` onto `onto` for a restack, without touching any
    /// branch. The commits keep their authors and messages, and `name`/`email` commit them.
    pub async fn prepare_restack(
        &self,
        from: &str,
        head: &str,
        onto: &str,
        name: &str,
        email: &str,
    ) -> Result<String, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.prepare_restack(from, head, onto, name, email).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrepareRestack {
                            from: from.into(),
                            head: head.into(),
                            onto: onto.into(),
                            name: name.into(),
                            email: email.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Moves each branch in `moves` to its new commit, provided each is still at its old one and
    /// `base_branch` is still at `base`. Nothing moves unless every check passes.
    pub async fn publish_restack_as(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        base_branch: &str,
        base: &str,
        moves: &[RestackMove],
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .publish_restack(pool, repo_id, actor, base_branch, base, moves)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishRestack {
                            base_branch: base_branch.into(),
                            base: base.into(),
                            moves: moves.to_vec(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    // ----- Git sessions -----

    /// The first bytes of a smart-HTTP or SSH session: the refs this repository
    /// offers to a client of `service`.
    pub async fn advertise_refs(&self, service: GitService) -> Result<Vec<u8>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.advertise_refs(service).await,
            Backend::Remote(remote) => {
                remote
                    .bytes(Operation::AdvertiseRefs { service }, b"")
                    .await
            }
        }
    }

    /// Pipes the client to Git on the owning node, with the shared receive hooks.
    pub async fn git_session(
        &self,
        session: GitSession,
        receive: Option<(
            crate::backend::db::DbPool,
            std::sync::Arc<crate::backend::ci::CiRuns>,
        )>,
        streams: GitStreams<'_>,
    ) -> anyhow::Result<std::process::ExitStatus> {
        match &self.0 {
            Backend::Local(local) => local.git_session(session, receive, streams).await,
            Backend::Remote(remote) => remote.git_session(session, streams).await,
        }
    }

    /// The branch HEAD names on the owning node (`refs/heads/main`), also when unborn.
    pub async fn head_target(&self) -> Result<Option<String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.head_target().await,
            Backend::Remote(_) => Err(rpc::wire::invalid("HEAD is read on the owning node")),
        }
    }

    /// Streams the objects that the listed refs need beyond the tips a copy holds.
    pub async fn replica_pack(
        &self,
        targets: &[String],
        have: &[String],
        streams: GitStreams<'_>,
    ) -> anyhow::Result<std::process::ExitStatus> {
        match &self.0 {
            Backend::Local(local) => local.replica_pack(targets, have, streams).await,
            Backend::Remote(_) => {
                Err(rpc::wire::invalid("replica packs come from the primary").into())
            }
        }
    }

    /// Stores a pack from the primary in this copy.
    pub async fn index_pack(&self, pack: &Path) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.index_pack(pack).await,
            Backend::Remote(_) => Err(rpc::wire::invalid("replica copies live on this node")),
        }
    }

    /// Makes this copy's refs exactly `refs`.
    pub async fn set_refs(&self, refs: &HashMap<String, String>) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.set_refs(refs).await,
            Backend::Remote(_) => Err(rpc::wire::invalid("replica copies live on this node")),
        }
    }

    /// Points this copy's HEAD at the primary's branch.
    pub async fn set_head_target(&self, target: &str) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.set_head_target(target).await,
            Backend::Remote(_) => Err(rpc::wire::invalid("replica copies live on this node")),
        }
    }

    /// The local directory for the host OpenSSH streaming gateway only.
    /// HTTP and built-in SSH use streamed store sessions instead.
    pub fn local_path_for_spawning_git(&self) -> Result<&Path, RepoError> {
        match &self.0 {
            Backend::Local(local) => Ok(local.path()),
            Backend::Remote(_) => Err(rpc::wire::invalid(
                "Host OpenSSH cannot serve remote repositories; use HTTP or built-in SSH",
            )),
        }
    }
}

#[cfg(test)]
mod tests;
