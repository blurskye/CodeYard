use crate::store_types::AsyncMergePublication;
use crate::store_types::GitService;
use crate::store_types::GitSession;
use crate::store_types::MergeFailure;
use crate::store_types::MergeMethod;
use crate::store_types::MergeRequest;
use crate::store_types::PullBranchUpdate;
use crate::store_types::RestackMove;
use anyhow::Result;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::CommitFilter;
use codeyard_repos::editor::FileWrite;
use codeyard_repos::editor::PreparedFile;
use codeyard_repos::editor::UploadWrite;
use codeyard_repos::editor::MAX_EDITOR_BYTES;
use codeyard_repos::editor::MAX_UPLOAD_TOTAL_BYTES;
use codeyard_storage::range::ByteRange as BlobRange;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

pub const MAX_BYTES: usize = 128 * 1024 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(300);

/// A first page leaves its zero offset out, so an older node answers it as before.
fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// A retained credential uses the former numeric token slot, so an older storage peer
/// rejects it instead of silently dropping a new optional authorization field. A bare token
/// number from an older peer carries no secret and fails to decode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicationCredential(codeyard_db_core::queries::credentials::Actor);
impl PublicationCredential {
    /// Trusted in-process callers carry no credential, and the receiving node uses the caller.
    pub fn retain(actor: codeyard_db_core::queries::credentials::Actor) -> Option<Self> {
        actor.credential.is_some().then_some(Self(actor))
    }
    pub fn into_actor(
        self,
        expected: i64,
    ) -> Result<codeyard_db_core::queries::credentials::Actor, RepoError> {
        let Self(actor) = self;
        if actor.user_id != expected {
            return Err(invalid("publication credential actor mismatch"));
        }
        Ok(actor)
    }
}

/// Bind a mutation's original text input to its initiating credential. Keeping the
/// credential in a formerly string-valued slot makes older peers reject it safely.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PublicationText {
    Legacy(String),
    Retained {
        value: String,
        authority: codeyard_db_core::queries::credentials::Actor,
    },
}
impl From<&str> for PublicationText {
    fn from(value: &str) -> Self {
        Self::Legacy(value.into())
    }
}
impl From<String> for PublicationText {
    fn from(value: String) -> Self {
        Self::Legacy(value)
    }
}
impl PublicationText {
    pub fn retain(value: &str, authority: codeyard_db_core::queries::credentials::Actor) -> Self {
        if authority.credential.is_none() {
            return Self::Legacy(value.into());
        }
        Self::Retained {
            value: value.into(),
            authority,
        }
    }
    pub fn into_parts(
        self,
        actor: i64,
    ) -> Result<(codeyard_db_core::queries::credentials::Actor, String), RepoError> {
        match self {
            Self::Legacy(value) => Ok((actor.into(), value)),
            Self::Retained { value, authority } => {
                Ok((PublicationCredential(authority).into_actor(actor)?, value))
            }
        }
    }
}

/// Filtered listings use an object in the existing ref slot: older peers reject the
/// request rather than silently returning an unfiltered prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FilePathQuery {
    Reference(String),
    Filtered { reference: String, query: String },
}
impl From<&str> for FilePathQuery {
    fn from(reference: &str) -> Self {
        Self::Reference(reference.into())
    }
}
impl From<String> for FilePathQuery {
    fn from(reference: String) -> Self {
        Self::Reference(reference)
    }
}
impl FilePathQuery {
    pub fn into_parts(self) -> (String, String) {
        match self {
            Self::Reference(reference) => (reference, String::new()),
            Self::Filtered { reference, query } => (reference, query),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Operation {
    ObjectPut {
        declaration: codeyard_storage::nodes::Declaration,
    },
    ObjectGet {
        key: String,
        offset: u64,
        length: Option<u64>,
    },
    ObjectDelete {
        key: String,
    },
    Initialize {
        source: Option<Uuid>,
        default_branch: String,
    },
    /// Kept separate from Initialize so older nodes reject new copy semantics rather than
    /// silently preserving history when a template requested its removal.
    InitializeCopy {
        source: Uuid,
        default_branch: PublicationText,
        template: bool,
        default_branch_only: bool,
    },
    Remove,
    Retire,
    GistSnapshot {
        revision: Option<String>,
    },
    GistHistory {
        revision: Option<String>,
        offset: usize,
        limit: usize,
    },
    GistWrite {
        credential: codeyard_repos::gists::GistCredential,
    },
    /// Fetches a commit of another repository on this node into a pull ref.
    FetchPullHead {
        token: Option<i64>,
        source: Uuid,
        number: i32,
        head: String,
        expected_head: Option<String>,
    },
    Exists,
    SizeBytes,
    RefreshStorageSize,
    Refs,
    MatchingRefs {
        prefix: String,
    },
    /// The same refs as `Refs`, answered only by the primary. For a caller that writes the
    /// answer into the database.
    PrimaryRefs,
    /// Default-branch statistics are computed and cached by the owning primary.
    PrimaryLanguages {
        git_ref: String,
    },
    BranchPublication {
        repository_id: i64,
        reference: String,
    },
    Info,
    CommitCount,
    GitTree {
        revision: String,
        recursive: bool,
        #[serde(default)]
        raw_json: bool,
    },
    GitTag {
        sha: String,
        #[serde(default)]
        raw_json: bool,
    },
    GitObjectKind {
        sha: String,
    },
    IntroducedBlobs {
        sha: String,
    },
    UpdateReference {
        reference: String,
        sha: PublicationText,
        force: bool,
    },
    WriteGitObject {
        input: codeyard_git::objects::ObjectWrite,
        token: Option<i64>,
    },
    ImportGit {
        import: codeyard_repos::imports::GitImport,
    },
    CancelImport,
    LockMigration {
        migration: i64,
    },
    MigrationArchive,
    Tree {
        git_ref: String,
        path: String,
        #[serde(default)]
        metadata_only: bool,
    },
    SidebarTrees {
        git_ref: String,
        paths: Vec<String>,
    },
    WorkflowFiles {
        revision: String,
    },
    TemplateTexts {
        revision: String,
        paths: Vec<String>,
        #[serde(default)]
        raw_json: bool,
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
    BlobByIdRange {
        id: String,
        range: BlobRange,
    },
    Commits {
        git_ref: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "CommitFilter::is_empty")]
        filter: CommitFilter,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        walk_millis: Option<u64>,
        /// The caller can decode JSON from the raw response frame.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        raw_json: bool,
    },
    CommitPage {
        git_ref: String,
        offset: usize,
        limit: usize,
        #[serde(default, skip_serializing_if = "CommitFilter::is_empty")]
        filter: CommitFilter,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        walk_millis: Option<u64>,
        /// The caller can decode JSON from the raw response frame.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        raw_json: bool,
    },
    /// Lines of the files at `git_ref` that contain `text`, as `RepoStore::search_code` finds them.
    SearchCode {
        git_ref: String,
        text: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: usize,
        limit: usize,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        raw_json: bool,
    },
    /// Commits that changed `path`, as `git log -- <path>` lists them.
    PathCommits {
        git_ref: String,
        path: String,
        offset: usize,
        limit: usize,
        #[serde(default, skip_serializing_if = "CommitFilter::is_empty")]
        filter: CommitFilter,
        /// The caller can decode JSON from the raw response frame.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        raw_json: bool,
    },
    Commit {
        revision: String,
    },
    /// The same commit as `Commit`, answered only by the primary. For a caller that writes
    /// the answer into the database.
    PrimaryCommit {
        revision: String,
    },
    ReleaseHistory {
        head: String,
        previous: Option<String>,
        exclude_tag: String,
    },
    CommitDiff {
        revision: String,
    },
    Diff {
        base_ref: String,
        head_ref: String,
        /// Counts and file metadata only, with empty patches.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stats: bool,
    },
    /// Commits reachable from `head_ref` but not from `base_ref`, newest first.
    CommitsBetween {
        base_ref: String,
        head_ref: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: usize,
        limit: usize,
        /// Include the full count and the requested window of GitHub's oldest 250 commits.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pull_request: bool,
        /// The caller can decode JSON from the raw response frame.
        #[serde(default)]
        raw_json: bool,
    },
    DiffFile {
        base_ref: String,
        head_ref: String,
        path: String,
    },
    ChangedPaths {
        base_ref: String,
        head_ref: String,
        #[serde(default)]
        endpoints: bool,
        #[serde(default)]
        renames: bool,
    },
    ComparisonFrom {
        source: Uuid,
        base_ref: String,
        head_ref: String,
        offset: usize,
        limit: usize,
        /// The caller can decode JSON from the raw response frame.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        raw_json: bool,
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
        format: codeyard_git::ArchiveFormat,
    },
    Languages {
        git_ref: String,
    },
    FilePaths {
        git_ref: FilePathQuery,
        limit: usize,
        #[serde(default)]
        raw_json: bool,
    },
    Blame {
        git_ref: String,
        path: String,
    },
    Contributors,
    BranchContributors {
        branch: String,
    },
    CommitStatistics {
        branch: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        raw_json: bool,
        #[serde(default)]
        mode: codeyard_git::StatisticsMode,
    },
    CodeFrequency {
        branch: String,
    },
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
    RenameBranch {
        old: String,
        new: PublicationText,
    },
    #[serde(rename = "MergeUpstreamWithCredential", alias = "MergeUpstream")]
    MergeUpstream {
        source: Uuid,
        branch: String,
        #[serde(default)]
        token: Option<i64>,
    },
    CreateBranch {
        name: String,
        sha: PublicationText,
    },
    DeleteBranch {
        name: PublicationText,
    },
    SetDefaultBranch {
        name: PublicationText,
    },
    TransferRepository {
        new_owner: PublicationText,
    },
    #[serde(
        rename = "ConfigureDependencySecurityWithAuthority",
        alias = "ConfigureDependencySecurity"
    )]
    ConfigureDependencySecurity {
        credential: codeyard_db_core::queries::users::accounts::OwnedAccountCredential,
        feature: codeyard_db_repos::repositories::dependencies::Feature,
        enabled: bool,
        private_consent: bool,
    },
    MutateRelease {
        mutation: crate::repositories::releases::immutable::Mutation,
        attestation: Option<codeyard_model::release_integrity::Attestation>,
    },
    PatchRepository {
        default_branch: PublicationText,
        changes: codeyard_db_repos::repositories::records::RepositoryChanges<String>,
    },
    CreateTag {
        name: String,
        sha: PublicationText,
        message: Option<String>,
        tagger_name: String,
        tagger_email: String,
    },
    DeleteTag {
        name: PublicationText,
    },
    PrepareFile {
        write: FileWrite,
        author: String,
        email: String,
        #[serde(default)]
        identities: codeyard_repos::editor::CommitIdentities,
    },
    #[serde(rename = "PublishFileWithAuthority", alias = "PublishFile")]
    PublishFile {
        prepared: PreparedFile,
    },
    #[serde(
        rename = "PublishDependencyUpdateWithAuthority",
        alias = "PublishDependencyUpdate"
    )]
    PublishDependencyUpdate {
        prepared: PreparedFile,
        publication: Box<codeyard_db_repos::repositories::dependencies::Publication>,
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
    PrepareRevert {
        request: crate::store_types::RevertRequest,
    },
    UpdatePullBranch {
        source: Uuid,
        request: PullBranchUpdate,
    },
    PublishAsyncMerge {
        request: AsyncMergePublication,
    },
    #[serde(
        rename = "PublishBranchMergeWithAuthority",
        alias = "PublishBranchMerge"
    )]
    PublishBranchMerge {
        publication: crate::store_types::BranchMergePublication,
    },
    PublishMerge {
        head_branch: String,
        base_branch: String,
        head: String,
        base: String,
        result: String,
        method: MergeMethod,
        #[serde(default)]
        token: Option<PublicationCredential>,
        /// The merge policy revision the merge was checked against; the publish refuses a newer one.
        #[serde(default)]
        policy: Option<codeyard_db_repos::pull_requests::merge_policy::Snapshot>,
    },
    /// Publishes a stacked merge commit. Each head is `(head branch or pull ref, commit)`.
    PublishStackedMerge {
        heads: Vec<(String, String)>,
        base_branch: String,
        base: String,
        result: String,
        #[serde(default)]
        token: Option<PublicationCredential>,
        /// The merge policy revision the merge was checked against; the publish refuses a newer one.
        #[serde(default)]
        policy: Option<codeyard_db_repos::pull_requests::merge_policy::Snapshot>,
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
    /// checked against `base` and the stack against the `versions` the rebase read.
    PublishRestack {
        stack_id: i64,
        versions: Vec<(i64, i64)>,
        base_branch: PublicationText,
        base: String,
        moves: Vec<RestackMove>,
    },
    AdvertiseRefs {
        service: GitService,
    },
    GitSession(GitSession),
    /// A fork exports only visible branches and tags under its initiating credential.
    ForkPack {
        authority: codeyard_db_core::queries::credentials::Actor,
        default_branch_only: bool,
    },
    /// A replica copy asks its primary for the objects it lacks. `have` lists the tips the
    /// copy already holds. The reply lists HEAD and refs before the object stream.
    ReplicaPack {
        have: Vec<String>,
        /// Commits named by an operator's database snapshot, even if their refs moved.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        want: Vec<String>,
    },
    PrepareWrite {
        id: String,
    },
    ApplyWrite {
        id: String,
    },
    WritePack {
        id: String,
    },
}

impl Operation {
    pub fn is_object(&self) -> bool {
        matches!(
            self,
            Self::ObjectPut { .. } | Self::ObjectGet { .. } | Self::ObjectDelete { .. }
        )
    }
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
            | Self::GitTree { .. }
            | Self::GitTag { .. }
            | Self::GitObjectKind { .. }
            | Self::IntroducedBlobs { .. }
            | Self::SidebarTrees { .. }
            | Self::TemplateTexts { .. }
            | Self::Blob { .. }
            | Self::BlobBytes { .. }
            | Self::BlobById { .. }
            | Self::BlobByIdRange { .. }
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
            | Self::MatchingRefs { .. }
            | Self::Info
            | Self::CommitCount
            | Self::Tags
            | Self::Archive { .. }
            | Self::Languages { .. }
            | Self::FilePaths { .. }
            | Self::Blame { .. }
            | Self::Contributors
            | Self::BranchContributors { .. }
            | Self::CommitStatistics { .. }
            | Self::CodeFrequency { .. }
            | Self::BranchStatuses { .. }
            | Self::GistSnapshot { .. }
            | Self::GistHistory { .. }
            | Self::ForkPack { .. } => true,
            Self::AdvertiseRefs { service } => *service == GitService::UploadPack,
            Self::GitSession(session) => session.service == GitService::UploadPack,
            Self::ObjectPut { .. } | Self::ObjectGet { .. } | Self::ObjectDelete { .. } => false,
            Self::Exists
            | Self::SizeBytes
            | Self::WorkflowFiles { .. }
            | Self::PrimaryRefs
            | Self::PrimaryLanguages { .. }
            | Self::BranchPublication { .. }
            | Self::PrimaryCommit { .. }
            | Self::ReleaseHistory { .. }
            | Self::ChangedPaths { .. }
            | Self::DiffFrom { .. }
            | Self::ComparisonFrom { .. }
            | Self::IsAncestor { .. }
            | Self::ResolveCiCommit { .. }
            | Self::CiWorkflowFile { .. } => false,
            Self::Initialize { .. }
            | Self::InitializeCopy { .. }
            | Self::Remove
            | Self::Retire
            | Self::FetchPullHead { .. }
            | Self::PrepareFile { .. }
            | Self::WriteGitObject { .. }
            | Self::GistWrite { .. }
            | Self::ImportGit { .. }
            | Self::CancelImport
            | Self::LockMigration { .. }
            | Self::MigrationArchive
            | Self::UpdateReference { .. }
            | Self::PrepareUpload { .. }
            | Self::PublishFile { .. }
            | Self::PublishDependencyUpdate { .. }
            | Self::SaveFile { .. }
            | Self::Merge { .. }
            | Self::PrepareMerge { .. }
            | Self::PrepareRevert { .. }
            | Self::PublishMerge { .. }
            | Self::PublishAsyncMerge { .. }
            | Self::UpdatePullBranch { .. }
            | Self::PublishBranchMerge { .. }
            | Self::PublishStackedMerge { .. }
            | Self::PrepareRestack { .. }
            | Self::PublishRestack { .. }
            | Self::RenameBranch { .. }
            | Self::MergeUpstream { .. }
            | Self::CreateBranch { .. }
            | Self::DeleteBranch { .. }
            | Self::SetDefaultBranch { .. }
            | Self::PatchRepository { .. }
            | Self::MutateRelease { .. }
            | Self::TransferRepository { .. }
            | Self::ConfigureDependencySecurity { .. }
            | Self::CreateTag { .. }
            | Self::DeleteTag { .. }
            | Self::RefreshStorageSize
            | Self::ReplicaPack { .. }
            | Self::PrepareWrite { .. }
            | Self::ApplyWrite { .. }
            | Self::WritePack { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiRunAccess {
    pub id: i64,
    pub worker: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct GitExit {
    pub result: Result<i32, RepoError>,
    pub stderr: String,
}

pub fn invalid(message: &str) -> RepoError {
    RepoError::new(ErrorKind::Invalid, message)
}

pub fn unavailable() -> RepoError {
    RepoError::storage_node_unavailable()
}

/// The request-body limit for an operation. All other buffered operations are bodyless.
pub fn operation_body_limit(operation: &Operation) -> usize {
    match operation {
        Operation::ObjectPut { declaration } => declaration
            .bytes
            .clamp(0, codeyard_support::object_limits::MAX_OBJECT_BYTES)
            as usize,
        Operation::PrepareFile { .. } | Operation::SaveFile { .. } => MAX_EDITOR_BYTES,
        Operation::WriteGitObject {
            input: codeyard_git::objects::ObjectWrite::Blob,
            ..
        } => codeyard_git::objects::MAX_BLOB_WRITE_BYTES,
        Operation::PrepareUpload { .. } => MAX_UPLOAD_TOTAL_BYTES,
        Operation::GistWrite { .. } => codeyard_git::gist_types::MAX_GIST_REQUEST_BYTES,
        Operation::ImportGit { .. } => codeyard_repos::imports::MAX_IMPORT_PACK_BYTES,
        _ => 0,
    }
}

pub fn repository_uuid(path: &str) -> Result<Uuid, RepoError> {
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("invalid repository UUID"))?;
    let uuid = filename
        .strip_suffix(".git")
        .ok_or_else(|| invalid("invalid repository UUID"))?;
    Uuid::parse_str(uuid).map_err(|_| invalid("invalid repository UUID"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_cluster_core::link::{read_frame, write_frame};
    use codeyard_db_core::limits::MAX_JSON;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn unavailable_keeps_its_typed_reason_on_the_wire() {
        let error = unavailable();
        assert!(error.is_storage_node_unavailable());
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "storage node unavailable");
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "Unavailable",
                "reason": "StorageNodeUnavailable",
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
                walk_millis: None,
                raw_json: false,
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
                walk_millis: None,
                raw_json: false,
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
            raw_json: false,
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
            Operation::CreateBranch { name, sha: PublicationText::Legacy(sha) }
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
                identities: Default::default(),
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
        use codeyard_cluster_core::transport::ALPN_RPC;
        use codeyard_support::node_identity::NodeKey;
        let server_key = NodeKey::generate();
        let client_key = NodeKey::generate();
        let (client_io, server_io) = tokio::io::duplex(capacity);
        let acceptor = tokio_rustls::TlsAcceptor::from(
            codeyard_cluster_core::tls::server_config(&server_key).unwrap(),
        );
        let connector = tokio_rustls::TlsConnector::from(
            codeyard_cluster_core::tls::client_config(
                &client_key,
                server_key.public_key(),
                ALPN_RPC,
            )
            .unwrap(),
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
        let copying = tokio::spawn(codeyard_git::repository::process::copy(
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
