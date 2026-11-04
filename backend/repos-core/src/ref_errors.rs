use codeyard_git::error::{RepoError, MERGE_COMMIT_BLOCKED};

#[derive(Debug)]
pub enum RefError {
    Access(codeyard_identity::repository_access::AccessError),
    Invalid(&'static str),
    /// A rule of the branch refuses the change; the text says which.
    Protected(&'static str),
    Unavailable,
    Git(RepoError),
    AppliedFollowupFailed,
}

pub fn publication_error(error: RepoError) -> RefError {
    if error.is_applied_followup_failed() {
        RefError::AppliedFollowupFailed
    } else if error.is_protected_branch() {
        RefError::Protected("This branch is protected")
    } else if error.message == crate::repositories::releases::immutable::TAG_PROTECTED {
        RefError::Protected(crate::repositories::releases::immutable::TAG_PROTECTED)
    } else if error.is_merge_commit_blocked() {
        RefError::Protected(MERGE_COMMIT_BLOCKED)
    } else if error.is_write_denied() {
        RefError::Access(codeyard_identity::repository_access::AccessError::WriteDenied)
    } else {
        RefError::Git(error)
    }
}

