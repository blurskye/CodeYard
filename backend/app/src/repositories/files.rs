//! Web editor and API file writes: check who may write where, then have the store
//! commit the file and move the branch, failing if it changed since the editor loaded.
use crate::ci::push::RefUpdate;
use crate::ci::runs::CiRuns;
use crate::cluster::context::NodeContext;
use crate::repositories::protection::check_branch_write;
use crate::repositories::protection::BranchWrite;
use crate::repositories::protection::History;
use crate::repositories::protection::Refusal;
use crate::repositories::protection::WriteVia;
use crate::repositories::protection::ANY_TIP;
use crate::repositories::store::editor::CommitIdentities;
use crate::repositories::store::editor::FileWrite;
use crate::repositories::store::editor::PreparedFile;
use crate::repositories::store::editor::UploadWrite;
use crate::repositories::store::editor::MAX_EDITOR_BYTES;
use crate::repositories::store::repo_store;
use crate::repositories::store::RepoStore;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::CommitInfo;
use std::collections::HashMap;

pub struct FileSave {
    pub commit: CommitInfo,
    pub actor: codeyard_db_core::models::User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFailure {
    Invalid,
    Conflict,
    Unavailable,
    NotFound,
    Denied,
    TooLarge,
    Encoding,
    SavedFollowupFailed,
}

/// Decode Git content with bounded normalization, accepting conventional base64 whitespace.
pub fn decode_base64(content: &str, limit: usize) -> Result<Vec<u8>, WriteFailure> {
    use base64::Engine;
    let encoded_limit = limit.div_ceil(3).saturating_mul(4);
    let mut normalized = Vec::with_capacity(content.len().min(encoded_limit));
    for character in content
        .chars()
        .filter(|character| !character.is_whitespace())
    {
        if normalized.len() >= encoded_limit {
            return Err(WriteFailure::TooLarge);
        }
        if !character.is_ascii() {
            return Err(WriteFailure::Encoding);
        }
        normalized.push(character as u8);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&normalized)
        .map_err(|_| WriteFailure::Encoding)?;
    if bytes.len() > limit {
        return Err(WriteFailure::TooLarge);
    }
    Ok(bytes)
}

#[derive(Debug)]
pub struct WriteError {
    pub kind: WriteFailure,
    pub message: String,
}
fn conflict() -> WriteError {
    WriteError { kind: WriteFailure::Conflict, message: "The branch changed since this editor was loaded. Your text is preserved. Open the latest file in another tab and reconcile before saving.".into() }
}
/// The branch named by the write no longer exists. Nothing moved, so this is a 400 that asks
/// for another branch, not a 409 that asks the user to reconcile.
fn missing_branch(branch: &str) -> WriteError {
    WriteError {
        kind: WriteFailure::Invalid,
        message: format!("Branch `{branch}` does not exist. Create it on the Branches page first, or save to an existing branch."),
    }
}
fn unavailable() -> WriteError {
    WriteError {
        kind: WriteFailure::Unavailable,
        message: "Repository storage is unavailable. The file was not saved.".into(),
    }
}
fn write_error(error: RepoError) -> WriteError {
    if error.is_applied_followup_failed() {
        return WriteError {
            kind: WriteFailure::SavedFollowupFailed,
            message: "The change was saved, but default branch synchronization failed. Check the repository before retrying.".into(),
        };
    }
    if error.is_write_denied() {
        return WriteError {
            kind: WriteFailure::Denied,
            message: "You no longer have write access to this repository.".into(),
        };
    }
    if error.is_protected_branch() {
        return WriteError {
            kind: WriteFailure::Denied,
            message: "This branch is protected and its rules do not allow this change.".into(),
        };
    }
    if error.is_storage_node_unavailable() {
        return WriteError {
            kind: WriteFailure::Unavailable,
            message: "storage node unavailable".into(),
        };
    }
    match error.kind {
        ErrorKind::Invalid => WriteError {
            kind: WriteFailure::Invalid,
            message: error.message,
        },
        ErrorKind::Conflict => conflict(),
        _ => unavailable(),
    }
}

/// Original text suitable for the native editor, without replacing invalid bytes.
pub fn editor_text(bytes: &[u8], is_binary: bool) -> Result<&str, WriteError> {
    if is_binary || bytes.contains(&0) || bytes.len() > MAX_EDITOR_BYTES {
        return Err(WriteError {
            kind: WriteFailure::TooLarge,
            message: "Only text files up to 1 MiB can be edited in this native editor.".into(),
        });
    }
    std::str::from_utf8(bytes).map_err(|_| WriteError {
        kind: WriteFailure::Encoding,
        message: "The native editor supports UTF-8 text only. Use the file API to preserve other encodings.".into(),
    })
}

// Form submissions normalize line breaks. Use the original file's first newline
// style; new files and files without a newline use LF.
fn native_file_bytes(content: String, original: &str) -> Vec<u8> {
    native_text(content, original).into_bytes()
}

pub fn native_text(content: String, original: &str) -> String {
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    if content == original.replace("\r\n", "\n").replace('\r', "\n") {
        return original.to_owned();
    }
    let newline = original.find(['\r', '\n']).map(|index| &original[index..]);
    match newline {
        Some(rest) if rest.starts_with("\r\n") => content.replace('\n', "\r\n"),
        Some(rest) if rest.starts_with('\r') => content.replace('\n', "\r"),
        _ => content,
    }
}

enum FileContent {
    Text(String),
    Bytes(Vec<u8>),
}
#[derive(Clone)]
enum CommitOrigin {
    Web,
    Api(CommitIdentities),
}

async fn check_web_signoff(
    pool: &codeyard_db_core::DbPool,
    repository: &codeyard_db_core::models::Repository,
    user: &codeyard_db_core::models::User,
    message: &str,
) -> Result<(), WriteError> {
    let required = codeyard_db_orgs::organizations::settings::repository_requires_web_signoff(
        pool,
        repository.id,
    )
    .await
    .map_err(|_| unavailable())?
    .ok_or(WriteError {
        kind: WriteFailure::NotFound,
        message: "Repository not found.".into(),
    })?;
    let email = user.commit_email();
    if required
        && !codeyard_db_orgs::organizations::settings::has_signoff(message, &user.username, &email)
    {
        return Err(WriteError {
            kind: WriteFailure::Invalid,
            message: codeyard_db_orgs::organizations::settings::required_signoff_message(
                &user.username,
                &email,
            ),
        });
    }
    Ok(())
}

/// Save from the HTML editor, whose text is a string.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn write_file(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    owner: &str,
    repository_id: i64,
    input: FileWrite,
    content: String,
    new_branch: Option<&str>,
) -> Result<String, WriteError> {
    // A browser form turns every newline into CRLF, so the text can be twice the file. The
    // file's own size is checked once the newlines are restored.
    if content.len() > MAX_EDITOR_BYTES * 2 {
        return Err(WriteError {
            kind: WriteFailure::TooLarge,
            message: "The native editor supports files up to 1 MiB.".into(),
        });
    }
    save_file(
        pool,
        cluster,
        ci,
        actor_id,
        owner,
        repository_id,
        input,
        FileContent::Text(content),
        new_branch,
        CommitOrigin::Web,
    )
    .await
    .map(|saved| saved.commit.sha)
}

/// Whether the branch rules refuse `user` a direct commit to `branch` at `revision`. The pages
/// then offer a commit on a new branch instead, as GitHub does for a protected branch.
pub async fn direct_commit_refused(
    pool: &codeyard_db_core::DbPool,
    repository: &codeyard_db_core::models::Repository,
    user: &codeyard_db_core::models::User,
    branch: &str,
    revision: &str,
) -> bool {
    let write = BranchWrite {
        branch,
        old: (revision != "unborn").then_some(revision),
        new: Some(ANY_TIP),
        via: WriteVia::Direct,
    };
    matches!(
        check_branch_write(pool, repository, user, &write, &History::SingleCommit).await,
        Err(Refusal::Blocked(_))
    )
}

const MAX_SUGGESTED_USERNAME_BYTES: usize = 200;

/// The name a commit on a new branch gets when the user has not typed one: `<user>-patch-N` for
/// the lowest N that no branch in `refs` uses.
pub fn free_branch_name(username: &str, refs: &HashMap<String, String>) -> String {
    // A long username is cut at a character boundary so the whole ref stays within limits.
    let mut end = username.len().min(MAX_SUGGESTED_USERNAME_BYTES);
    while !username.is_char_boundary(end) {
        end -= 1;
    }
    let username = &username[..end];
    // Usernames have no path separators. Each occupied branch (or directory of branches)
    // therefore excludes at most one candidate, and all candidates share this one index.
    let occupied: std::collections::HashSet<&str> = refs
        .keys()
        .filter_map(|reference| reference.strip_prefix("refs/heads/"))
        .map(|branch| branch.split('/').next().unwrap_or(branch))
        .collect();
    (1..)
        .map(|number| format!("{username}-patch-{number}"))
        .find(|name| !occupied.contains(name.as_str()))
        .unwrap_or_default()
}

/// The name of a new branch must be a valid branch name.
fn check_new_branch_name(name: &str) -> Result<(), WriteError> {
    if codeyard_git::branch_ref(name).is_none() {
        return Err(WriteError {
            kind: WriteFailure::Invalid,
            message: "Choose a valid name for the new branch.".into(),
        });
    }
    Ok(())
}

/// Refuses a repository the actor cannot read and returns the id of one it can. A missing
/// repository and an unreadable one get the same answer, so no refusal reveals which private
/// names exist. The JSON handlers call this before they decode the body.
pub async fn ensure_readable(
    pool: &codeyard_db_core::DbPool,
    user: &codeyard_db_core::models::User,
    owner: &str,
    name: &str,
) -> Result<i64, WriteError> {
    let repository = codeyard_db_repos::repositories::records::get_repository(pool, owner, name)
        .await
        .map_err(|_| unavailable())?
        .ok_or(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        })?;
    if !codeyard_identity::repository_access::can_read(pool, Some(user), &repository)
        .await
        .map_err(|_| unavailable())?
    {
        return Err(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        });
    }
    Ok(repository.id)
}

/// The repository at `owner/name`. A missing one is refused like an unreadable one, by the
/// callers that check access.
pub async fn named_repository(
    pool: &codeyard_db_core::DbPool,
    owner: &str,
    name: &str,
) -> Result<codeyard_db_core::models::Repository, WriteError> {
    codeyard_db_repos::repositories::records::get_repository(pool, owner, name)
        .await
        .map_err(|_| unavailable())?
        .ok_or(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        })
}

/// Save byte-exact file contents from the JSON API.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn write_file_bytes(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    owner: &str,
    repository_id: i64,
    input: FileWrite,
    content: Vec<u8>,
    identities: CommitIdentities,
) -> Result<FileSave, WriteError> {
    // The caller passes the repository it authorized, so a rename cannot retarget the write.
    save_file(
        pool,
        cluster,
        ci,
        actor_id,
        owner,
        repository_id,
        input,
        FileContent::Bytes(content),
        None,
        CommitOrigin::Api(identities),
    )
    .await
}

/// Removes a file in one commit. It takes the same checks, branch choice and commit path as a
/// save. `input.deleting` must be set.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn delete_file(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    owner: &str,
    repository_id: i64,
    input: FileWrite,
    new_branch: Option<&str>,
    identities: CommitIdentities,
) -> Result<FileSave, WriteError> {
    save_file(
        pool,
        cluster,
        ci,
        actor_id,
        owner,
        repository_id,
        input,
        FileContent::Bytes(Vec::new()),
        new_branch,
        CommitOrigin::Api(identities),
    )
    .await
}

/// Native deletion retains byte-safe handling while applying web-only organization policies.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn delete_file_web(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    owner: &str,
    repository_id: i64,
    input: FileWrite,
    new_branch: Option<&str>,
) -> Result<FileSave, WriteError> {
    save_file(
        pool,
        cluster,
        ci,
        actor_id,
        owner,
        repository_id,
        input,
        FileContent::Bytes(Vec::new()),
        new_branch,
        CommitOrigin::Web,
    )
    .await
}

/// The commit message a save gets when the editor or the API leaves it blank. It is the text
/// the commit box shows as its placeholder.
pub fn default_commit_message(input: &FileWrite) -> String {
    if input.deleting {
        format!("Delete {}", input.path)
    } else if input.creating {
        "Create file".into()
    } else {
        format!("Update {}", input.path)
    }
}

/// The checks every file write makes before it reads the branch. The actor must be able to
/// read the repository, or it is refused like a missing one, and `check` (the body's own checks)
/// runs only after that. The actor must then be able to write the branch, and the branch's
/// protection rules must accept the commit.
async fn authorize_write(
    pool: &codeyard_db_core::DbPool,
    actor_id: i64,
    repository_id: i64,
    branch: &str,
    expected_revision: &str,
    check: impl FnOnce() -> Result<(), WriteError>,
) -> Result<
    (
        codeyard_db_core::models::User,
        codeyard_db_core::models::Repository,
    ),
    WriteError,
> {
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or(WriteError {
            kind: WriteFailure::Denied,
            message: "Sign in to edit this repository.".into(),
        })?;
    let repository =
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository_id)
            .await
            .map_err(|_| unavailable())?
            .ok_or(WriteError {
                kind: WriteFailure::NotFound,
                message: "Repository not found.".into(),
            })?;
    if !codeyard_identity::repository_access::can_read(pool, Some(&user), &repository)
        .await
        .map_err(|_| unavailable())?
    {
        return Err(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        });
    }
    check()?;
    if !codeyard_identity::repository_access::can_write(pool, &user, &repository)
        .await
        .map_err(|_| unavailable())?
    {
        return Err(WriteError {
            kind: WriteFailure::Denied,
            message: "You do not have write access to this repository.".into(),
        });
    }
    let old = (expected_revision != "unborn").then_some(expected_revision);
    let write = BranchWrite {
        branch,
        old,
        new: Some(ANY_TIP),
        via: WriteVia::Direct,
    };
    match check_branch_write(pool, &repository, &user, &write, &History::SingleCommit).await {
        Ok(_) => {}
        Err(Refusal::Blocked(reason)) => {
            return Err(WriteError {
                kind: WriteFailure::Denied,
                message: reason.into(),
            });
        }
        Err(Refusal::Unavailable) => return Err(unavailable()),
    }
    Ok((user, repository))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
async fn save_file(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    _owner: &str,
    repository_id: i64,
    mut input: FileWrite,
    content: FileContent,
    new_branch: Option<&str>,
    origin: CommitOrigin,
) -> Result<FileSave, WriteError> {
    let authority = actor_id.into();
    let actor_id = authority.user_id;
    if input.message.trim().is_empty() {
        input.message = default_commit_message(&input);
    }
    // A commit on a new branch changes no existing branch, so the rules judge its creation.
    let (guard_branch, guard_revision) = match new_branch {
        Some(name) => (name, "unborn"),
        None => (input.branch.as_str(), input.expected_revision.as_str()),
    };
    // Only a repository the actor can read reaches the body checks, so an unreadable
    // repository answers like a missing one whatever the body says.
    let (user, repository) = authorize_write(
        pool,
        actor_id,
        repository_id,
        guard_branch,
        guard_revision,
        || {
            input.validate().map_err(write_error)?;
            if let CommitOrigin::Api(identities) = &origin {
                identities.validate().map_err(write_error)?;
            }
            if let Some(name) = new_branch {
                check_new_branch_name(name)?;
            }
            let too_large = match &content {
                FileContent::Text(text) => text.len() > MAX_EDITOR_BYTES * 2,
                FileContent::Bytes(bytes) => bytes.len() > MAX_EDITOR_BYTES,
            };
            if too_large {
                return Err(WriteError {
                    kind: WriteFailure::TooLarge,
                    message: "File writes support at most 1 MiB.".into(),
                });
            }
            Ok(())
        },
    )
    .await?;
    if matches!(origin, CommitOrigin::Web) {
        check_web_signoff(pool, &repository, &user, &input.message).await?;
    }
    let store = repo_store(&repository, cluster);
    let content = match content {
        FileContent::Bytes(bytes) => bytes,
        FileContent::Text(text) if input.creating => native_file_bytes(text, ""),
        FileContent::Text(text) => {
            // Report stale forms before encoding errors. The store still checks the
            // expected revision when it moves the branch after writing the commit.
            let current = store
                .commit(&format!("refs/heads/{}", input.branch))
                .await
                .map_err(|error| {
                    if error.kind == ErrorKind::NotFound {
                        conflict()
                    } else {
                        write_error(error)
                    }
                })?;
            if current.sha != input.expected_revision {
                return Err(conflict());
            }
            let original = store
                .blob_bytes(&input.expected_revision, &input.path)
                .await
                .map_err(write_error)?;
            let original = editor_text(&original.bytes, original.is_binary)?;
            native_file_bytes(text, original)
        }
    };
    if content.len() > MAX_EDITOR_BYTES {
        return Err(WriteError {
            kind: WriteFailure::TooLarge,
            message: "File writes support at most 1 MiB.".into(),
        });
    }
    let email = user.commit_email();
    let defaults = CommitIdentities::default();
    let identities = match &origin {
        CommitOrigin::Web => &defaults,
        CommitOrigin::Api(identities) => identities,
    };
    let mut prepared = store
        .prepare_file_with_identities(&input, &content, &user.username, &email, identities)
        .await
        .map_err(|error| {
            if error.is_branch_missing() {
                missing_branch(&input.branch)
            } else {
                write_error(error)
            }
        })?;
    prepared.web_commit = matches!(origin, CommitOrigin::Web);
    prepared.authority = Some(authority);
    publish_commit(pool, &store, ci, &repository, user, &prepared, new_branch).await
}

/// Moves the branch to a prepared commit, or with `new_branch` creates that branch at it. Pull
/// requests that use the branch follow it, and push workflows are queued for the paths that
/// changed.
async fn publish_commit(
    pool: &codeyard_db_core::DbPool,
    store: &RepoStore,
    ci: &CiRuns,
    repository: &codeyard_db_core::models::Repository,
    user: codeyard_db_core::models::User,
    prepared: &PreparedFile,
    new_branch: Option<&str>,
) -> Result<FileSave, WriteError> {
    let (pool, store, ci, repository, prepared, new_branch) = (
        pool.clone(),
        store.clone(),
        ci.clone(),
        repository.clone(),
        prepared.clone(),
        new_branch.map(str::to_owned),
    );
    // Once publication starts, client cancellation must not abandon accepted-write follow-ups.
    tokio::spawn(async move {
    let (pool, store, ci, repository, prepared, new_branch) = (
        &pool, &store, &ci, &repository, &prepared, new_branch.as_deref(),
    );
    let mut accepted_followup_failed = false;
    let (branch, commit, old) = match new_branch {
        None => {
            let commit = match store.publish_file_as(pool, repository.id, user.id, prepared).await {
                Ok(commit) => commit,
                Err(error) if error.is_applied_followup_failed() => {
                    accepted_followup_failed = true;
                    prepared.commit.clone()
                }
                Err(error) => return Err(write_error(error)),
            };
            (prepared.branch.as_str(), commit, prepared.expected.clone())
        }
        Some(name) => {
            // The commit's parent is the tip the user saw, so the new branch starts from it.
            // Creating the branch fails if another branch took the name meanwhile.
            if store.commit(&format!("refs/heads/{name}")).await.is_ok() {
                return Err(WriteError {
                    kind: WriteFailure::Invalid,
                    message: format!("Branch `{name}` already exists. Choose another name."),
                });
            }
            let mut publication = prepared.clone();
            publication.branch = name.to_owned();
            publication.reference = format!("refs/heads/{name}");
            publication.expected = None;
            publication.new_branch = true;
            let commit = match store.publish_file_as(pool, repository.id, user.id, &publication).await {
                Ok(commit) => commit,
                Err(error) if error.is_applied_followup_failed() => {
                    accepted_followup_failed = true;
                    publication.commit.clone()
                }
                Err(error) => return Err(write_error(error)),
            };
            (name, commit, None)
        }
    };
    // The branch moved: pull requests that use it follow, and push workflows run.
    let current = codeyard_identity::repository_access::current_repository_with_owner(pool, repository.id).await;
    let followup_repository = current.as_ref().map(|(repo, _)| repo).unwrap_or(repository);
    let refreshed = crate::repositories::pull_requests::branches::refresh_open_pull_requests(
        pool,
        followup_repository,
        Some(&[branch][..]),
        ci,
        user.id,
        codeyard_db_core::models::AccessLevel::Owner,
    )
    .await
    .is_ok();
    if let Ok((repository, owner)) = current {
        ci.queue_push_runs(
            &repository,
            &owner,
            user.id,
            RefUpdate {
                reference: format!("refs/heads/{branch}"),
                old,
                new: commit.sha.clone(),
                changed_paths: None,
            },
        ).await;
    } else {
        accepted_followup_failed = true;
    }
    if !refreshed || accepted_followup_failed {
        return Err(WriteError {
            kind: WriteFailure::SavedFollowupFailed,
            message: "The change was saved, but repository follow-up failed. Check the repository before retrying.".into(),
        });
    }
    Ok(FileSave {
        commit,
        actor: user,
    })
    }).await.map_err(|_| WriteError {
        kind: WriteFailure::SavedFollowupFailed,
        message: "File publication was interrupted; check the branch before retrying.".into(),
    })?
}

/// Adds or replaces every file of an upload in one commit. `content` holds the files' bytes
/// back to back, in the order of `write.files`.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn upload_files(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    _owner: &str,
    repository_id: i64,
    write: UploadWrite,
    content: Vec<u8>,
    new_branch: Option<&str>,
) -> Result<FileSave, WriteError> {
    let authority = actor_id.into();
    let actor_id = authority.user_id;
    let (guard_branch, guard_revision) = match new_branch {
        Some(name) => (name, "unborn"),
        None => (write.branch.as_str(), write.expected_revision.as_str()),
    };
    let (user, repository) = authorize_write(
        pool,
        actor_id,
        repository_id,
        guard_branch,
        guard_revision,
        || {
            write.validate().map_err(write_error)?;
            if let Some(name) = new_branch {
                check_new_branch_name(name)?;
            }
            Ok(())
        },
    )
    .await?;
    let store = repo_store(&repository, cluster);
    let email = user.commit_email();
    check_web_signoff(pool, &repository, &user, &write.message).await?;
    let mut prepared = store
        .prepare_upload(&write, &content, &user.username, &email)
        .await
        .map_err(|error| {
            if error.is_branch_missing() {
                missing_branch(&write.branch)
            } else {
                write_error(error)
            }
        })?;
    prepared.web_commit = true;
    prepared.authority = Some(authority);
    publish_commit(pool, &store, ci, &repository, user, &prepared, new_branch).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_text_validation_never_replaces_invalid_bytes() {
        assert_eq!(editor_text("café\n".as_bytes(), false).unwrap(), "café\n");
        assert_eq!(
            editor_text(b"caf\xe9\n", false).unwrap_err().kind,
            WriteFailure::Encoding
        );
        assert_eq!(
            editor_text(b"binary\0", true).unwrap_err().kind,
            WriteFailure::TooLarge
        );
        assert_eq!(
            editor_text(&vec![b'x'; MAX_EDITOR_BYTES + 1], false)
                .unwrap_err()
                .kind,
            WriteFailure::TooLarge
        );
    }

    #[test]
    fn native_editor_rejects_nul_after_the_binary_probe() {
        let mut bytes = vec![b'a'; 9000];
        bytes.push(0);
        assert_eq!(
            editor_text(&bytes, false).unwrap_err().kind,
            WriteFailure::TooLarge
        );
    }

    #[test]
    fn a_new_branch_name_takes_the_first_free_patch_number() {
        let refs = HashMap::from([
            ("refs/heads/main".to_owned(), "a".repeat(40)),
            ("refs/heads/alice-patch-1".to_owned(), "b".repeat(40)),
        ]);
        assert_eq!(free_branch_name("alice", &refs), "alice-patch-2");
        assert_eq!(free_branch_name("bob", &refs), "bob-patch-1");
    }

    #[test]
    fn native_newlines_follow_the_original_file_or_default_to_lf() {
        let submitted = "one\r\ntwo\rthree\n";
        for (original, expected) in [
            ("", "one\ntwo\nthree\n"),
            ("no newline", "one\ntwo\nthree\n"),
            ("LF\n", "one\ntwo\nthree\n"),
            ("CRLF\r\n", "one\r\ntwo\r\nthree\r\n"),
            ("CR\r", "one\rtwo\rthree\r"),
        ] {
            assert_eq!(
                native_file_bytes(submitted.into(), original),
                expected.as_bytes()
            );
        }
        assert_eq!(
            native_file_bytes("no final newline".into(), "old\r\n"),
            b"no final newline"
        );
    }
}
