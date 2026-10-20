//! Web editor and API file writes: check who may write where, then have the store
//! commit the file and move the branch, failing if it changed since the editor loaded.
use crate::backend::{
    ci::{CiRuns, RefUpdate},
    cluster::NodeContext,
    db,
    repositories::{
        access,
        protection::{check_branch_write, BranchWrite, History, Refusal, WriteVia, ANY_TIP},
        pull_requests,
        store::{
            repo_store, CommitInfo, ErrorKind, FileWrite, PreparedFile, RepoError, RepoStore,
            UploadWrite, MAX_EDITOR_BYTES,
        },
    },
};
use std::collections::HashMap;

pub struct FileSave {
    pub commit: CommitInfo,
    pub actor: db::models::User,
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
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    let newline = original.find(['\r', '\n']).map(|index| &original[index..]);
    match newline {
        Some(rest) if rest.starts_with("\r\n") => content.replace('\n', "\r\n").into_bytes(),
        Some(rest) if rest.starts_with('\r') => content.replace('\n', "\r").into_bytes(),
        _ => content.into_bytes(),
    }
}

enum FileContent {
    Text(String),
    Bytes(Vec<u8>),
}

/// Save from the HTML editor, whose text is a string.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn write_file(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: i64,
    owner: &str,
    repository_id: i64,
    input: FileWrite,
    content: String,
    new_branch: Option<&str>,
) -> Result<String, WriteError> {
    if content.len() > MAX_EDITOR_BYTES {
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
    )
    .await
    .map(|saved| saved.commit.sha)
}

/// Whether the branch rules refuse `user` a direct commit to `branch` at `revision`. The pages
/// then offer a commit on a new branch instead, as GitHub does for a protected branch.
pub async fn direct_commit_refused(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    user: &db::models::User,
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

/// The name a commit on a new branch gets when the user has not typed one: `<user>-patch-N` for
/// the lowest N that no branch in `refs` uses.
pub fn free_branch_name(username: &str, refs: &HashMap<String, String>) -> String {
    (1..)
        .map(|number| format!("{username}-patch-{number}"))
        .find(|name| !refs.contains_key(&format!("refs/heads/{name}")))
        .unwrap_or_default()
}

/// The name of a new branch must be a valid branch name.
fn check_new_branch_name(name: &str) -> Result<(), WriteError> {
    if crate::backend::git::branch_ref(name).is_none() {
        return Err(WriteError {
            kind: WriteFailure::Invalid,
            message: "Choose a valid name for the new branch.".into(),
        });
    }
    Ok(())
}

/// Refuses a repository the actor cannot read. A missing repository and an unreadable one get
/// the same answer, so no refusal reveals which private names exist. The JSON handlers call
/// this before they decode the body.
pub async fn ensure_readable(
    pool: &db::DbPool,
    user: &db::models::User,
    owner: &str,
    name: &str,
) -> Result<(), WriteError> {
    let repository = db::repositories::records::get_repository(pool, owner, name)
        .await
        .map_err(|_| unavailable())?
        .ok_or(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        })?;
    if !access::can_read(pool, Some(user), &repository)
        .await
        .map_err(|_| unavailable())?
    {
        return Err(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        });
    }
    Ok(())
}

/// The repository at `owner/name`. A missing one is refused like an unreadable one, by the
/// callers that check access.
pub async fn named_repository(
    pool: &db::DbPool,
    owner: &str,
    name: &str,
) -> Result<db::models::Repository, WriteError> {
    db::repositories::records::get_repository(pool, owner, name)
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
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: i64,
    owner: &str,
    name: &str,
    input: FileWrite,
    content: Vec<u8>,
) -> Result<FileSave, WriteError> {
    let repository = named_repository(pool, owner, name).await?;
    save_file(
        pool,
        cluster,
        ci,
        actor_id,
        owner,
        repository.id,
        input,
        FileContent::Bytes(content),
        None,
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
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: i64,
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
    )
    .await
}

/// The commit message a save gets when the editor or the API leaves it blank. It is the text
/// the commit box shows as its placeholder.
fn default_commit_message(input: &FileWrite) -> String {
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
    pool: &db::DbPool,
    actor_id: i64,
    repository_id: i64,
    branch: &str,
    expected_revision: &str,
    check: impl FnOnce() -> Result<(), WriteError>,
) -> Result<(db::models::User, db::models::Repository), WriteError> {
    let user = db::users::accounts::get_user_by_id(pool, actor_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or(WriteError {
            kind: WriteFailure::Denied,
            message: "Sign in to edit this repository.".into(),
        })?;
    let repository = db::repositories::records::get_repository_by_id(pool, repository_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        })?;
    if !access::can_read(pool, Some(&user), &repository)
        .await
        .map_err(|_| unavailable())?
    {
        return Err(WriteError {
            kind: WriteFailure::NotFound,
            message: "Repository not found.".into(),
        });
    }
    check()?;
    if !access::can_write(pool, &user, &repository)
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
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: i64,
    owner: &str,
    repository_id: i64,
    mut input: FileWrite,
    content: FileContent,
    new_branch: Option<&str>,
) -> Result<FileSave, WriteError> {
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
            if let Some(name) = new_branch {
                check_new_branch_name(name)?;
            }
            let size = match &content {
                FileContent::Text(text) => text.len(),
                FileContent::Bytes(bytes) => bytes.len(),
            };
            if size > MAX_EDITOR_BYTES {
                return Err(WriteError {
                    kind: WriteFailure::TooLarge,
                    message: "File writes support at most 1 MiB.".into(),
                });
            }
            Ok(())
        },
    )
    .await?;
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
    let prepared = store
        .prepare_file(&input, &content, &user.username, &email)
        .await
        .map_err(|error| {
            if error.is_branch_missing() {
                missing_branch(&input.branch)
            } else {
                write_error(error)
            }
        })?;
    publish_commit(
        pool,
        &store,
        ci,
        owner,
        &repository,
        user,
        &prepared,
        new_branch,
    )
    .await
}

/// Moves the branch to a prepared commit, or with `new_branch` creates that branch at it. Pull
/// requests that use the branch follow it, and push workflows are queued for the paths that
/// changed.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
async fn publish_commit(
    pool: &db::DbPool,
    store: &RepoStore,
    ci: &CiRuns,
    owner: &str,
    repository: &db::models::Repository,
    user: db::models::User,
    prepared: &PreparedFile,
    new_branch: Option<&str>,
) -> Result<FileSave, WriteError> {
    let (branch, commit, old) = match new_branch {
        None => {
            let commit = store
                .publish_file_as(pool, repository.id, user.id, prepared)
                .await
                .map_err(write_error)?;
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
            store
                .create_branch_as(pool, repository.id, user.id, name, &prepared.commit.sha)
                .await
                .map_err(write_error)?;
            (name, prepared.commit.clone(), None)
        }
    };
    // The branch moved: pull requests that use it follow, and push workflows run.
    let refreshed = pull_requests::refresh_open_pull_requests(
        pool,
        repository,
        Some(&[branch][..]),
        ci,
        user.id,
        crate::backend::db::models::AccessLevel::Owner,
    )
    .await
    .is_ok();
    ci.queue_push_runs(
        repository,
        owner,
        user.id,
        RefUpdate {
            reference: format!("refs/heads/{branch}"),
            old,
            new: commit.sha.clone(),
            changed_paths: None,
        },
    )
    .await;
    if !refreshed {
        return Err(WriteError {
            kind: WriteFailure::SavedFollowupFailed,
            message: "The change was saved, but pull request refresh failed. Check the repository before retrying.".into(),
        });
    }
    Ok(FileSave {
        commit,
        actor: user,
    })
}

/// Adds or replaces every file of an upload in one commit. `content` holds the files' bytes
/// back to back, in the order of `write.files`.
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn upload_files(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor_id: i64,
    owner: &str,
    repository_id: i64,
    write: UploadWrite,
    content: Vec<u8>,
    new_branch: Option<&str>,
) -> Result<FileSave, WriteError> {
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
    let prepared = store
        .prepare_upload(&write, &content, &user.username, &email)
        .await
        .map_err(|error| {
            if error.is_branch_missing() {
                missing_branch(&write.branch)
            } else {
                write_error(error)
            }
        })?;
    publish_commit(
        pool,
        &store,
        ci,
        owner,
        &repository,
        user,
        &prepared,
        new_branch,
    )
    .await
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
