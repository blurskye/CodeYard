//! The error every store call returns, and the reasons that travel with it between nodes.
use crate::backend::repositories::protection::MERGE_COMMIT_BLOCKED;
use serde::{Deserialize, Serialize};
use std::fmt;

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

/// Refused when a replica asks for a copy while a ref change to the repository is in progress.
/// The copy waits and asks again on its next pass.
pub const REPLICA_BUSY: &str = "Repository writes are in progress.";

/// The failures that callers act on. The reason travels with the error between nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoErrorReason {
    NotFile,
    StorageNodeUnavailable,
    SessionsBusy,
    ReplicaBusy,
    WriteDenied,
    ProtectedBranch,
    MergeCommitBlocked,
    AuthorityUnavailable,
    AppliedFollowupFailed,
    ResponseTooLarge,
    RangeNotSatisfiable,
    BranchMissing,
    MergeMethodNotAllowed,
    StackChanged,
}

/// A failed store call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoError {
    pub kind: ErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RepoErrorReason>,
    /// What Git said, or a fixed sentence for requests refused here.
    pub message: String,
}

impl RepoError {
    pub fn is_storage_node_unavailable(&self) -> bool {
        self.reason == Some(RepoErrorReason::StorageNodeUnavailable)
    }

    pub fn is_sessions_busy(&self) -> bool {
        self.reason == Some(RepoErrorReason::SessionsBusy)
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

    pub fn is_stack_changed(&self) -> bool {
        self.reason == Some(RepoErrorReason::StackChanged)
    }

    pub fn is_replica_busy(&self) -> bool {
        self.reason == Some(RepoErrorReason::ReplicaBusy)
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

    pub fn replica_busy() -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            reason: Some(RepoErrorReason::ReplicaBusy),
            message: REPLICA_BUSY.into(),
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

    /// A branch protection rule refused the write. `message` is the rule's own text.
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

    /// A layer of the stack being rebased was retargeted, closed, merged, unstacked or joined
    /// since the rebase read it.
    pub fn stack_changed() -> Self {
        Self {
            kind: ErrorKind::Conflict,
            reason: Some(RepoErrorReason::StackChanged),
            message: "The stack changed while it was being rebased.".into(),
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

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RepoError {}

pub(super) fn invalid_ref(kind: &str) -> RepoError {
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
        if let Some(conflict) = error.downcast_ref::<crate::backend::git::repository::RefConflict>()
        {
            return Self::new(ErrorKind::Conflict, conflict.to_string());
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
        if error.is::<crate::backend::git::repository::NotFile>() {
            return Self {
                kind: ErrorKind::MissingPath,
                reason: Some(RepoErrorReason::NotFile),
                message: "The path is not a file".into(),
            };
        }
        if error.is::<crate::backend::git::InvalidByteRange>() {
            return Self::range_not_satisfiable();
        }
        if let Some(refused) = error.downcast_ref::<crate::backend::git::repository::BlameRefused>()
        {
            return Self::new(ErrorKind::Invalid, refused.0);
        }
        let kind = match error.downcast_ref::<git2::Error>() {
            Some(error) if crate::backend::git::is_missing_path(error) => ErrorKind::MissingPath,
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
        // A request for a ref or commit this repository lacks is reported with its own words.
        let message = match error.downcast_ref::<git2::Error>() {
            Some(named)
                if named.code() == git2::ErrorCode::InvalidSpec
                    && named.class() == git2::ErrorClass::Invalid =>
            {
                named.message().to_owned()
            }
            _ => error.to_string(),
        };
        Self::new(kind, message)
    }
}
