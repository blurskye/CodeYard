//! Creating repositories: the authorized database row, then its Git storage.
use crate::backend::webhooks::{self, events, payloads};
use crate::backend::{
    cluster::{placement, NodeContext},
    db::{self, models::Repository, repositories::creation as repo_lifecycle},
    repositories::{
        store::{repo_store, ErrorKind, RepoError, UploadFile, UploadWrite},
        templates::{self, Template},
    },
};
use chrono::{Datelike, Utc};
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleFailure {
    Invalid,
    OwnRepositoryFork,
    Denied,
    UnsupportedOrganizationFork,
    Conflict,
    Unavailable,
    InitializationCleanupFailed,
    DeletedStorageRemains,
}

#[derive(Debug)]
pub struct StorageError {
    pub kind: LifecycleFailure,
    pub message: &'static str,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for StorageError {}

fn storage_error(error: anyhow::Error) -> StorageError {
    match error.downcast_ref::<repo_lifecycle::RepositoryStorageRefusal>() {
        Some(repo_lifecycle::RepositoryStorageRefusal::Denied) => StorageError {
            kind: LifecycleFailure::Denied,
            message: "You are not allowed to create this repository.",
        },
        Some(repo_lifecycle::RepositoryStorageRefusal::UnsupportedOrganizationFork) => StorageError {
            kind: LifecycleFailure::UnsupportedOrganizationFork,
            message: "Organization forks are not supported; create an empty organization repository instead.",
        },
        Some(repo_lifecycle::RepositoryStorageRefusal::StalePlacement) => unavailable(),
        None => match error.downcast_ref::<db::repositories::records::RepositorySettingsRefusal>() {
            Some(db::repositories::records::RepositorySettingsRefusal::Denied | db::repositories::records::RepositorySettingsRefusal::VisibilityLocked) => StorageError { kind: LifecycleFailure::Denied, message: "Repository mutation is not permitted." },
            Some(db::repositories::records::RepositorySettingsRefusal::InvalidName) => StorageError { kind: LifecycleFailure::Invalid, message: "Invalid repository name." },
            Some(db::repositories::records::RepositorySettingsRefusal::NameConflict) => StorageError { kind: LifecycleFailure::Conflict, message: "A repository with this name already exists." },
            Some(db::repositories::records::RepositorySettingsRefusal::NoMergeMethod) => StorageError { kind: LifecycleFailure::Invalid, message: "At least one merge method must stay allowed." },
            Some(db::repositories::records::RepositorySettingsRefusal::InvalidTopics) => StorageError { kind: LifecycleFailure::Invalid, message: "Invalid repository topics." },
            Some(db::repositories::records::RepositorySettingsRefusal::InvalidWebsite) => StorageError { kind: LifecycleFailure::Invalid, message: "Invalid repository website." },
            None => unavailable(),
        },
    }
}

fn unavailable() -> StorageError {
    StorageError {
        kind: LifecycleFailure::Unavailable,
        message: "Repository storage is unavailable. Try again.",
    }
}

async fn prepare_local_repository_root(path: &Path) -> Result<(), StorageError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|_| unavailable())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|_| unavailable())?;
    }
    Ok(())
}

fn git_storage_error(error: RepoError) -> StorageError {
    if error.is_storage_node_unavailable() {
        StorageError {
            kind: LifecycleFailure::Unavailable,
            message: "storage node unavailable",
        }
    } else {
        unavailable()
    }
}

async fn cleanup_failed_initialization(
    pool: &db::DbPool,
    context: Option<&NodeContext>,
    actor: i64,
    repository: &Repository,
) -> (bool, bool) {
    // Never remove storage while a live database row could still point at it.
    // Re-read the row because a delete may commit even when its response is lost.
    let deleted = db::repositories::records::delete_repository(pool, actor, repository.id).await;
    let row_removed =
        match db::repositories::records::get_repository_by_id(pool, repository.id).await {
            Ok(row) => row.is_none(),
            Err(_) => false,
        };
    if !row_removed {
        return (false, false);
    }

    // The deleted row names the storage that holds the files. When the response was lost,
    // the row from creation is the best record left.
    let placement = deleted.as_ref().unwrap_or(repository);
    let storage_removed = repo_store(placement, context).remove().await;
    if let Err(error) = &storage_removed {
        tracing::error!(
            repo_id = placement.id,
            storage_node_id = ?placement.storage_node_id,
            disk_path = %placement.disk_path,
            %error,
            "failed repository initialization storage cleanup"
        );
    }
    (true, storage_removed.is_ok())
}

/// UUID directory names never change on a rename or ownership transfer.
pub async fn provision(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: repo_lifecycle::RepositoryCreationOwner,
    name: &str,
    description: Option<&str>,
    private: bool,
    source: Option<&Repository>,
) -> Result<Repository, StorageError> {
    validate_name(name)?;
    let (pool, context) = node;
    let storage_node_id = placement::storage_node(context, source)
        .await
        .map_err(git_storage_error)?;
    let root = std::path::absolute(repos_path).map_err(|_| unavailable())?;
    let location = root.join(format!("{}.git", Uuid::new_v4()));
    let disk_path = location.to_str().ok_or_else(unavailable)?;
    let repository = repo_lifecycle::insert_repository(
        pool,
        repo_lifecycle::NewRepository {
            owner,
            name,
            description,
            is_private: private,
            disk_path,
            storage_node_id,
            source_id: source.map(|repo| repo.id),
        },
    )
    .await
    .map_err(storage_error)?
    .ok_or(StorageError {
        kind: LifecycleFailure::Conflict,
        message: "A repository with this name already exists.",
    })?;
    let store = repo_store(&repository, context);
    let initialized = async {
        // Local creation used to make the root before initializing the repository.
        if storage_node_id.is_none()
            || context.is_some_and(|node| storage_node_id == Some(node.node_id))
        {
            prepare_local_repository_root(&root).await?;
        }
        if let Some(source) = source {
            store
                .initialize_fork(
                    pool,
                    repository.id,
                    source.id,
                    owner.actor_id,
                    &repo_store(source, context),
                    &repository.default_branch,
                )
                .await
                .map_err(|error| StorageError {
                    kind: if error.kind == ErrorKind::Invalid {
                        LifecycleFailure::Denied
                    } else {
                        LifecycleFailure::Unavailable
                    },
                    message: "Repository is not available for forking.",
                })?;
        } else {
            store
                .initialize(None, &repository.default_branch)
                .await
                .map_err(git_storage_error)?;
        }
        // Fork initialization records its version on the owning store before refreshing languages.
        if source.is_none() {
            db::cluster::bump_write_version(pool, repository.id, None)
                .await
                .map_err(|_| unavailable())?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = initialized {
        return Err(abandon_repository(pool, context, owner.actor_id, &repository, error).await);
    }
    let mut repository = repository;
    if source.is_some() {
        // The fork is already usable. Accounting failures must not report creation as failed.
        let accounting = async {
            let size = store.size_bytes().await?.min(i64::MAX as u64) as i64;
            db::repositories::records::update_repo_size(pool, repository.id, size).await?;
            Ok::<_, anyhow::Error>(size)
        }
        .await;
        match accounting {
            Ok(size) => repository.size_bytes = size,
            Err(error) => {
                tracing::warn!(repo_id = repository.id, %error, "fork created but size accounting failed")
            }
        }
    }
    Ok(repository)
}

/// The files a new repository's first commit holds, as its creator chose them. The default
/// holds nothing, which leaves the repository empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InitialFiles {
    pub readme: bool,
    pub gitignore: Option<&'static Template>,
    pub license: Option<&'static Template>,
}

impl InitialFiles {
    fn is_empty(&self) -> bool {
        !self.readme && self.gitignore.is_none() && self.license.is_none()
    }
}

/// Creates a repository as [`provision`] does and, when `files` asks for any, commits them as
/// the first commit on the default branch. Once storage is published, later failures leave it intact.
pub async fn provision_with_initial_files(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: repo_lifecycle::RepositoryCreationOwner,
    name: &str,
    description: Option<&str>,
    private: bool,
    files: InitialFiles,
) -> Result<Repository, StorageError> {
    let repository = provision(node, repos_path, owner, name, description, private, None).await?;
    if files.is_empty() {
        return Ok(repository);
    }
    let (pool, context) = node;
    commit_initial_files(
        pool,
        context,
        owner.actor_id,
        &repository,
        description,
        files,
    )
    .await
    .map_err(|_| StorageError {
        kind: LifecycleFailure::Unavailable,
        message: "The repository was created, but its initial files could not be committed. The repository and any accepted writes were preserved.",
    })?;
    Ok(repository)
}

async fn commit_initial_files(
    pool: &db::DbPool,
    context: Option<&NodeContext>,
    actor: i64,
    repository: &Repository,
    description: Option<&str>,
    files: InitialFiles,
) -> Result<(), StorageError> {
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    // The license names the display name when there is one, and never a line break.
    let holder = user
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.contains(['\r', '\n']))
        .unwrap_or(&user.username);
    let year = Utc::now().year();
    let mut texts: Vec<(&str, String)> = Vec::new();
    if files.readme {
        texts.push((
            "README.md",
            templates::readme(&repository.name, description),
        ));
    }
    if let Some(template) = files.gitignore {
        texts.push((".gitignore", template.text.to_owned()));
    }
    if let Some(template) = files.license {
        texts.push((
            "LICENSE",
            templates::fill_license(template.text, year, holder),
        ));
    }
    let write = UploadWrite {
        branch: repository.default_branch.clone(),
        message: "Initial commit".into(),
        expected_revision: "unborn".into(),
        files: texts
            .iter()
            .map(|(path, text)| UploadFile {
                path: (*path).into(),
                size: text.len(),
            })
            .collect(),
    };
    let content: Vec<u8> = texts.iter().flat_map(|(_, text)| text.bytes()).collect();
    let store = repo_store(repository, context);
    let prepared = store
        .prepare_upload(&write, &content, &user.username, &user.commit_email())
        .await
        .map_err(git_storage_error)?;
    store
        .publish_file_as(pool, repository.id, actor, &prepared)
        .await
        .map_err(git_storage_error)?;
    Ok(())
}

/// Removes a repository whose setup failed, and returns the error to report. When the removal
/// is incomplete, that failure is reported instead, because the repository may still exist.
async fn abandon_repository(
    pool: &db::DbPool,
    context: Option<&NodeContext>,
    actor: i64,
    repository: &Repository,
    error: StorageError,
) -> StorageError {
    // Network and disk work are never performed while a DB transaction is held.
    let (row_removed, storage_removed) =
        cleanup_failed_initialization(pool, context, actor, repository).await;
    if row_removed && storage_removed {
        return error;
    }
    tracing::error!(
        repo_id = repository.id,
        storage_node_id = ?repository.storage_node_id,
        disk_path = %repository.disk_path,
        row_removed,
        storage_removed,
        "failed repository creation cleanup incomplete"
    );
    StorageError {
        kind: LifecycleFailure::InitializationCleanupFailed,
        message: "Repository initialization failed and cleanup was incomplete. Check your repositories before retrying.",
    }
}

pub async fn visibility_locked(
    pool: &db::DbPool,
    repository: &Repository,
) -> Result<bool, StorageError> {
    repo_lifecycle::repository_visibility_locked(pool, repository.id)
        .await
        .map_err(|_| unavailable())
}

pub fn validate_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty()
        || name.len() > 100
        || matches!(name, "." | "..")
        || name.ends_with(".git")
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(StorageError { kind: LifecycleFailure::Invalid, message: "Repository name must be 1-100 bytes and contain only letters, numbers, hyphens, underscores and dots; not . or .. and not ending in .git" });
    }
    Ok(())
}

/// Creation and updates check the description the user sends. A fork copies its
/// source's description without this check, so a fork can hold a longer one.
pub fn validate_description(description: &str) -> Result<(), StorageError> {
    if description.contains('\0') {
        return Err(StorageError {
            kind: LifecycleFailure::Invalid,
            message: "Description must not contain NUL bytes.",
        });
    }
    if description.len() > 2_000 {
        return Err(StorageError {
            kind: LifecycleFailure::Invalid,
            message: "Description must be at most 2000 bytes.",
        });
    }
    Ok(())
}

pub async fn delete(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    actor: i64,
    repo_id: i64,
) -> Result<(), StorageError> {
    super::access::check_admin(pool, repo_id, actor)
        .await
        .map_err(|error| StorageError {
            kind: if error == super::access::AccessError::Unavailable {
                LifecycleFailure::Unavailable
            } else {
                LifecycleFailure::Denied
            },
            message: "Repository deletion is not available.",
        })?;
    // Remove files where the deleted row says they are. A promotion may have moved the
    // primary after the check above.
    let deleted = db::repositories::records::delete_repository(pool, actor, repo_id)
        .await
        .map_err(storage_error)?;
    remove_deleted_repository(cluster, &deleted).await
}

pub(crate) async fn remove_deleted_repository(
    cluster: Option<&NodeContext>,
    deleted: &Repository,
) -> Result<(), StorageError> {
    if let Err(error) = repo_store(deleted, cluster).remove().await {
        tracing::error!(
            repo_id = deleted.id,
            storage_node_id = ?deleted.storage_node_id,
            disk_path = %deleted.disk_path,
            %error,
            "deleted repository storage remains"
        );
        return Err(StorageError { kind: LifecycleFailure::DeletedStorageRemains, message: "The repository was deleted, but its stored files could not be removed. An administrator has to clean up the storage." });
    }
    Ok(())
}

/// Moves a repository to another owner. Its id and storage stay the same, so only its clone
/// URL and web path change. The database checks admin rights on the repository and create
/// rights on the target.
pub async fn transfer(
    pool: &db::DbPool,
    actor: i64,
    repo_id: i64,
    new_owner: &str,
) -> Result<Repository, StorageError> {
    let target = transfer_target(pool, new_owner).await?;
    db::repositories::records::transfer_repository(pool, actor, repo_id, target)
        .await
        .map_err(storage_error)
}

async fn transfer_target(
    pool: &db::DbPool,
    handle: &str,
) -> Result<repo_lifecycle::RepositoryTarget, StorageError> {
    if let Some(user) = db::users::accounts::get_user_by_username(pool, handle)
        .await
        .map_err(|_| unavailable())?
    {
        return Ok(repo_lifecycle::RepositoryTarget::Personal(user.id));
    }
    if let Some(org) = db::organizations::profiles::get_public_organization(pool, handle)
        .await
        .map_err(|_| unavailable())?
    {
        return Ok(repo_lifecycle::RepositoryTarget::Organization(org.id));
    }
    Err(StorageError {
        kind: LifecycleFailure::Invalid,
        message: "No user or organization has that name.",
    })
}

/// Why `actor` may not fork `source`, or `None` when the fork may go ahead. The fork action
/// and the repository page's Fork control share these rules. The caller checks read access,
/// and the database refuses archived sources again when it inserts the fork.
pub fn fork_refusal(source: &Repository, actor: i64) -> Option<StorageError> {
    if source.is_personal_owner(actor) {
        Some(StorageError {
            kind: LifecycleFailure::OwnRepositoryFork,
            message: "Cannot fork your own repository",
        })
    } else if source.is_archived {
        Some(StorageError {
            kind: LifecycleFailure::Denied,
            message: "Archived repositories cannot be forked",
        })
    } else {
        None
    }
}

pub async fn fork(
    pool: &db::DbPool,
    context: Option<&NodeContext>,
    repos_path: &Path,
    broadcaster: &crate::backend::notifications::NotificationBroadcaster,
    actor: i64,
    source_id: i64,
    owner: &str,
) -> Result<Repository, StorageError> {
    let source = super::access::check_read(pool, source_id, Some(actor))
        .await
        .map_err(|error| StorageError {
            kind: if error == super::access::AccessError::Unavailable {
                LifecycleFailure::Unavailable
            } else {
                LifecycleFailure::Denied
            },
            message: "Repository is not available for forking.",
        })?;
    if let Some(refusal) = fork_refusal(&source, actor) {
        return Err(refusal);
    }
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    let fork = provision(
        (pool, context),
        repos_path,
        repo_lifecycle::RepositoryCreationOwner::personal(actor),
        &source.name,
        source.description.as_deref(),
        source.is_private,
        Some(&source),
    )
    .await?;
    if let Some(recipient) = source.personal_owner_id().filter(|id| *id != actor) {
        let title = format!("{} forked {}/{}", user.username, owner, source.name);
        let body = format!("New fork: {}/{}", user.username, source.name);
        let link = format!("/{}/{}", user.username, source.name);
        let _ = crate::backend::notifications::create::create_and_broadcast_notification(pool, broadcaster, crate::backend::notifications::create::NotificationRequest {
            user_id: recipient, actor_id: Some(actor), repo_id: Some(source.id), kind: "repository_forked", title: &title, body: Some(&body), link: Some(&link), metadata: serde_json::json!({"source_owner": owner, "source_repo": source.name, "subject": format!("{owner}/{}", source.name), "fork_owner": user.username, "fork_repo": fork.name}),
        }).await;
    }
    let (fork_for_hook, source_for_hook) = (fork.clone(), source.clone());
    webhooks::emit(
        pool,
        &source,
        events::FORK,
        vec![actor],
        move |source_owner, people| {
            let forker = people.get(actor);
            let forkee_owner = forker.login.clone();
            payloads::fork(
                &fork_for_hook,
                &forker,
                &source_for_hook,
                source_owner,
                &forkee_owner,
            )
        },
    );
    Ok(fork)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_reject_nul_before_storage() {
        assert!(validate_description("ordinary description").is_ok());
        assert_eq!(
            validate_description("bad\0text").unwrap_err().kind,
            LifecycleFailure::Invalid
        );
    }

    #[test]
    fn cli_error_context_preserves_storage_failure_kind() {
        let error = anyhow::Error::from(StorageError {
            kind: LifecycleFailure::DeletedStorageRemains,
            message: "The repository was deleted, but its stored files could not be removed.",
        })
        .context("Repository deletion failed");
        let storage = error.downcast_ref::<StorageError>().unwrap();
        assert_eq!(storage.kind, LifecycleFailure::DeletedStorageRemains);
        assert_eq!(storage.to_string(), storage.message);
    }
}
