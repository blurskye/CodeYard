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

pub(crate) fn storage_error(error: anyhow::Error) -> StorageError {
    if error.is::<db::users::accounts::AccountRestricted>() {
        return StorageError { kind: LifecycleFailure::Denied, message: "A live unrestricted account credential is required." };
    }
    if let Some(refusal) = error.downcast_ref::<db::organizations::properties::Refusal>() {
        return match refusal {
            db::organizations::properties::Refusal::Invalid { message, .. } => StorageError {
                kind: LifecycleFailure::Invalid,
                message,
            },
            db::organizations::properties::Refusal::Denied => StorageError {
                kind: LifecycleFailure::Denied,
                message: "Organization policy does not allow setting these repository properties.",
            },
            _ => StorageError {
                kind: LifecycleFailure::Invalid,
                message: "Invalid organization repository properties.",
            },
        };
    }
    match error.downcast_ref::<repo_lifecycle::RepositoryStorageRefusal>() {
        Some(repo_lifecycle::RepositoryStorageRefusal::Denied) => StorageError {
            kind: LifecycleFailure::Denied,
            message: "You are not allowed to create this repository.",
        },
        Some(repo_lifecycle::RepositoryStorageRefusal::StalePlacement) => unavailable(),
        None => {
            match error.downcast_ref::<db::repositories::records::RepositorySettingsRefusal>() {
                Some(
                    db::repositories::records::RepositorySettingsRefusal::Denied
                    | db::repositories::records::RepositorySettingsRefusal::VisibilityLocked,
                ) => StorageError {
                    kind: LifecycleFailure::Denied,
                    message: "Repository mutation is not permitted.",
                },
                Some(db::repositories::records::RepositorySettingsRefusal::InvalidName) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "Invalid repository name.",
                    }
                }
                Some(db::repositories::records::RepositorySettingsRefusal::NameConflict) => {
                    StorageError {
                        kind: LifecycleFailure::Conflict,
                        message: "A repository with this name already exists.",
                    }
                }
                Some(db::repositories::records::RepositorySettingsRefusal::NoMergeMethod) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "At least one merge method must stay allowed.",
                    }
                }
                Some(db::repositories::records::RepositorySettingsRefusal::InvalidTopics) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "Invalid repository topics.",
                    }
                }
                Some(db::repositories::records::RepositorySettingsRefusal::InvalidWebsite) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "Invalid repository website.",
                    }
                }
                None => unavailable(),
            }
        }
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
    repository: &Repository,
) -> (bool, bool) {
    // Never remove storage while a live database row could still point at it.
    // Re-read the row because a delete may commit even when its response is lost.
    // A row that has accepted writes is kept, so a failed creation never discards a push.
    let deleted = if repository.is_gist {
        db::gists::discard(pool, repository.id)
            .await
            .and_then(|row| {
                row.ok_or_else(|| anyhow::anyhow!("Gist initialization is no longer discardable"))
            })
    } else {
        db::repositories::records::delete_unwritten_repository(pool, repository.id)
            .await
            .and_then(|row| {
                row.ok_or_else(|| anyhow::anyhow!("Repository has accepted writes and is kept"))
            })
    };
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

/// Authority and initial organization metadata, validated together before storage is initialized.
#[derive(Clone, Copy, Debug)]
pub struct CreationInput<'a> {
    pub owner: repo_lifecycle::RepositoryCreationOwner,
    pub properties: &'a [db::organizations::properties::ValueInput],
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
    provision_with_properties(
        node,
        repos_path,
        CreationInput {
            owner,
            properties: &[],
        },
        name,
        description,
        private,
        source,
    )
    .await
}
pub async fn provision_with_properties(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    input: CreationInput<'_>,
    name: &str,
    description: Option<&str>,
    private: bool,
    source: Option<&Repository>,
) -> Result<Repository, StorageError> {
    provision_stored(
        node,
        repos_path,
        input,
        name,
        description,
        ProvisionOptions {
            private,
            creation_source: source.map(|repository| CreationSource::Fork {
                repository,
                default_branch_only: false,
            }),
            gist: false,
        },
    )
    .await
}
pub async fn provision_gist(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: repo_lifecycle::RepositoryCreationOwner,
    name: &str,
    source: Option<&Repository>,
) -> Result<Repository, StorageError> {
    provision_stored(
        node,
        repos_path,
        CreationInput {
            owner,
            properties: &[],
        },
        name,
        None,
        ProvisionOptions {
            private: false,
            creation_source: source.map(|repository| CreationSource::Fork {
                repository,
                default_branch_only: false,
            }),
            gist: true,
        },
    )
    .await
}
#[derive(Clone, Copy)]
enum CreationSource<'a> {
    Fork {
        repository: &'a Repository,
        default_branch_only: bool,
    },
    Template {
        repository: &'a Repository,
        all_branches: bool,
    },
}
impl<'a> CreationSource<'a> {
    fn repository(self) -> &'a Repository {
        match self {
            Self::Fork { repository, .. } | Self::Template { repository, .. } => repository,
        }
    }
}

async fn provision_from(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: repo_lifecycle::RepositoryCreationOwner,
    name: &str,
    description: Option<&str>,
    options: ProvisionOptions<'_>,
    chosen: &[db::organizations::properties::ValueInput],
) -> Result<Repository, StorageError> {
    let mut properties = inherited_properties(node.0, owner, options.creation_source).await?;
    // Values chosen in the creation form replace inherited ones.
    properties.retain(|value| {
        !chosen
            .iter()
            .any(|chosen| chosen.property_name == value.property_name)
    });
    properties.extend_from_slice(chosen);
    provision_stored(
        node,
        repos_path,
        CreationInput {
            owner,
            properties: &properties,
        },
        name,
        description,
        options,
    )
    .await
}

/// A copy made inside the source's organization keeps the property values its creator may set.
/// Another owner has its own definitions, so the copy starts from that owner's defaults.
async fn inherited_properties(
    pool: &db::DbPool,
    owner: repo_lifecycle::RepositoryCreationOwner,
    source: Option<CreationSource<'_>>,
) -> Result<Vec<db::organizations::properties::ValueInput>, StorageError> {
    let (Some(source), repo_lifecycle::RepositoryTarget::Organization(org)) =
        (source.map(CreationSource::repository), owner.target)
    else {
        return Ok(Vec::new());
    };
    if source.owner_org_id != Some(org) {
        return Ok(Vec::new());
    }
    let Some(db::organizations::properties::RepositoryMetadata {
        values,
        definitions,
        organization_owner,
        ..
    }) = db::organizations::properties::repository(pool, Some(owner.actor_id), source.id)
        .await
        .map_err(|_| unavailable())?
    else {
        return Ok(Vec::new());
    };
    Ok(values
        .properties
        .into_iter()
        .filter(|value| {
            value.value.is_some()
                && definitions.iter().any(|definition| {
                    definition
                        .fields
                        .property_name
                        .eq_ignore_ascii_case(&value.property_name)
                        && (organization_owner
                            || definition.fields.values_editable_by
                                == db::organizations::properties::EditableBy::OrgAndRepoActors)
                })
        })
        .collect())
}

struct ProvisionOptions<'a> {
    private: bool,
    creation_source: Option<CreationSource<'a>>,
    gist: bool,
}
async fn provision_stored(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    creation: CreationInput<'_>,
    name: &str,
    description: Option<&str>,
    options: ProvisionOptions<'_>,
) -> Result<Repository, StorageError> {
    let ProvisionOptions {
        private,
        creation_source,
        gist,
    } = options;
    let template = matches!(creation_source, Some(CreationSource::Template { .. }));
    let source = creation_source.map(CreationSource::repository);
    let owner = creation.owner;
    validate_name(name)?;
    let (pool, context) = node;
    let storage_node_id = placement::storage_node(context, source)
        .await
        .map_err(git_storage_error)?;
    let root = std::path::absolute(repos_path).map_err(|_| unavailable())?;
    let location = root.join(format!("{}.git", Uuid::new_v4()));
    let disk_path = location.to_str().ok_or_else(unavailable)?;
    let input = repo_lifecycle::NewRepository {
        owner,
        name,
        description,
        is_private: private,
        disk_path,
        storage_node_id,
        source_id: if template {
            None
        } else {
            source.map(|repo| repo.id)
        },
    };
    let mut repository = if gist {
        repo_lifecycle::insert_gist_repository(pool, input).await
    } else {
        repo_lifecycle::insert_repository_with_properties(pool, input, creation.properties).await
    }
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
            if template {
                db::repositories::records::update_repo_default_branch(
                    pool,
                    repository.id,
                    &source.default_branch,
                )
                .await
                .map_err(|_| unavailable())?;
                repository.default_branch = source.default_branch.clone();
            }
            let default_branch_only = match creation_source {
                Some(CreationSource::Fork {
                    default_branch_only,
                    ..
                }) => default_branch_only,
                Some(CreationSource::Template { all_branches, .. }) => !all_branches,
                None => false,
            };
            store
                .initialize_copy(
                    pool,
                    repository.id,
                    source.id,
                    owner.actor_id,
                    &repo_store(source, context),
                    &super::store::CopyOptions {
                        default_branch: repository.default_branch.clone(),
                        template,
                        default_branch_only,
                    },
                )
                .await
                .map_err(|error| StorageError {
                    kind: match error.kind {
                        ErrorKind::Invalid => LifecycleFailure::Denied,
                        ErrorKind::Conflict => LifecycleFailure::Conflict,
                        _ => LifecycleFailure::Unavailable,
                    },
                    message: if error.kind == ErrorKind::Conflict {
                        "Repository copy conflicts with reserved immutable tag names."
                    } else {
                        "Repository is not available for copying."
                    },
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
        return Err(abandon_repository(pool, context, &repository, error).await);
    }
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
    if source.is_none() {
        let created = repository.clone();
        let actor = owner.actor_id;
        webhooks::emit(
            pool,
            &repository,
            events::REPOSITORY,
            vec![actor],
            move |owner, people| {
                serde_json::json!({
                    "action": "created",
                    "repository": payloads::repository(&created, owner),
                    "sender": payloads::person(&people.get(actor)),
                })
            },
        )
        .await;
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
    provision_with_initial_properties(
        node,
        repos_path,
        CreationInput {
            owner,
            properties: &[],
        },
        name,
        description,
        private,
        files,
    )
    .await
}
pub async fn provision_with_initial_properties(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    input: CreationInput<'_>,
    name: &str,
    description: Option<&str>,
    private: bool,
    files: InitialFiles,
) -> Result<Repository, StorageError> {
    let owner = input.owner;
    let repository =
        provision_with_properties(node, repos_path, input, name, description, private, None)
            .await?;
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
/// is incomplete, that failure is reported instead, because the repository may still exist. A
/// repository that has accepted writes is kept and reported this way.
async fn abandon_repository(
    pool: &db::DbPool,
    context: Option<&NodeContext>,
    repository: &Repository,
    error: StorageError,
) -> StorageError {
    // Network and disk work are never performed while a DB transaction is held.
    let (row_removed, storage_removed) =
        cleanup_failed_initialization(pool, context, repository).await;
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
    actor: impl Into<db::users::accounts::AccountActor>,
    repo_id: i64,
) -> Result<(), StorageError> {
    let actor = actor.into();
    super::access::check_admin(pool, repo_id, actor.user_id())
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
    if let Err(error) = repo_store(deleted, cluster).retire().await {
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
    cluster: Option<&NodeContext>,
    actor: i64,
    repo_id: i64,
    new_owner: &str,
) -> Result<Repository, StorageError> {
    let repository = db::repositories::records::get_repository_by_id(pool, repo_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    repo_store(&repository, cluster)
        .transfer_repository_as(pool, repo_id, actor, new_owner)
        .await
        .map_err(|error| StorageError {
            kind: if error.is_write_denied() {
                LifecycleFailure::Denied
            } else {
                match error.kind {
                    ErrorKind::Conflict => LifecycleFailure::Conflict,
                    ErrorKind::Invalid => LifecycleFailure::Invalid,
                    _ => LifecycleFailure::Unavailable,
                }
            },
            message: if error.is_write_denied() {
                "Repository transfer denied."
            } else if error.kind == ErrorKind::Conflict {
                "Repository transfer conflicts with existing names or immutable tag history."
            } else {
                "Repository transfer unavailable; check the destination owner."
            },
        })
}

pub(crate) async fn transfer_target(
    pool: &db::DbPool,
    handle: &str,
) -> Result<(repo_lifecycle::RepositoryTarget, String), StorageError> {
    if let Some(user) = db::users::accounts::get_user_by_username(pool, handle)
        .await
        .map_err(|_| unavailable())?
    {
        return Ok((
            repo_lifecycle::RepositoryTarget::Personal(user.id),
            user.username,
        ));
    }
    if let Some(org) = db::organizations::profiles::get_public_organization(pool, handle)
        .await
        .map_err(|_| unavailable())?
    {
        return Ok((
            repo_lifecycle::RepositoryTarget::Organization(org.id),
            org.name,
        ));
    }
    Err(StorageError {
        kind: LifecycleFailure::Invalid,
        message: "No user or organization has that name.",
    })
}

/// Why `actor` may not fork `source`, or `None` when the fork may go ahead. The fork action
/// and the repository page's personal Fork control share this destination rule. The caller
/// checks read access; archived sources may be copied into a new writable repository.
pub fn fork_refusal(source: &Repository, actor: i64) -> Option<StorageError> {
    if source.is_personal_owner(actor) {
        Some(StorageError {
            kind: LifecycleFailure::OwnRepositoryFork,
            message: "Cannot fork your own repository",
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
    fork_into(
        pool,
        context,
        repos_path,
        broadcaster,
        ForkOptions {
            owner: repo_lifecycle::RepositoryCreationOwner::personal(actor),
            name: None,
            default_branch_only: false,
            properties: &[],
        },
        source_id,
        owner,
    )
    .await
}

pub struct ForkOptions<'a> {
    pub owner: repo_lifecycle::RepositoryCreationOwner,
    pub name: Option<&'a str>,
    pub default_branch_only: bool,
    /// Organization property values chosen for the copy.
    pub properties: &'a [db::organizations::properties::ValueInput],
}

pub async fn fork_into(
    pool: &db::DbPool,
    context: Option<&NodeContext>,
    repos_path: &Path,
    broadcaster: &crate::backend::notifications::NotificationBroadcaster,
    options: ForkOptions<'_>,
    source_id: i64,
    owner: &str,
) -> Result<Repository, StorageError> {
    let actor = options.owner.actor_id;
    repo_lifecycle::check_creation_authority(pool, options.owner)
        .await
        .map_err(storage_error)?;
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
    let same_owner = match options.owner.target {
        repo_lifecycle::RepositoryTarget::Personal(id) => source.is_personal_owner(id),
        repo_lifecycle::RepositoryTarget::Organization(id) => source.owner_org_id == Some(id),
    };
    if same_owner {
        return Err(StorageError {
            kind: LifecycleFailure::OwnRepositoryFork,
            message: "Cannot fork into the source owner.",
        });
    }
    if let Some(existing) = existing_fork(pool, actor, options.owner.target, source.id).await? {
        return Ok(existing);
    }
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    let fork_owner = match options.owner.target {
        repo_lifecycle::RepositoryTarget::Personal(_) => user.username.clone(),
        repo_lifecycle::RepositoryTarget::Organization(id) => {
            let db::organizations::OrganizationOutcome::Applied(org) =
                db::organizations::profiles::get_organization_metadata(pool, actor, id)
                    .await
                    .map_err(|_| unavailable())?
            else {
                return Err(StorageError {
                    kind: LifecycleFailure::Denied,
                    message: "Repository owner is unavailable.",
                });
            };
            org.name
        }
    };
    let fork = match provision_from(
        (pool, context),
        repos_path,
        options.owner,
        options.name.unwrap_or(&source.name),
        source.description.as_deref(),
        ProvisionOptions {
            private: source.is_private,
            creation_source: Some(CreationSource::Fork {
                repository: &source,
                default_branch_only: options.default_branch_only,
            }),
            gist: false,
        },
        options.properties,
    )
    .await
    {
        Ok(fork) => fork,
        Err(error) if error.kind == LifecycleFailure::Conflict => {
            if let Some(existing) =
                existing_fork(pool, actor, options.owner.target, source.id).await?
            {
                return Ok(existing);
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    if let Some(recipient) = source.personal_owner_id().filter(|id| *id != actor) {
        let title = format!("{} forked {}/{}", user.username, owner, source.name);
        let body = format!("New fork: {fork_owner}/{}", fork.name);
        let link = format!("/{fork_owner}/{}", fork.name);
        let notice = crate::backend::notifications::create::NotificationRequest {
            user_id: recipient,
            actor_id: Some(actor),
            repo_id: Some(source.id),
            kind: "repository_forked",
            title: &title,
            body: Some(&body),
            link: Some(&link),
            metadata: serde_json::json!({"source_owner":owner,"source_repo":source.name,"subject":format!("{owner}/{}",source.name),"fork_owner":fork_owner,"fork_repo":fork.name,"fork_repository_id":fork.id}),
        };
        let mut notified = Vec::new();
        if crate::backend::notifications::create::create_and_broadcast_notification(
            pool,
            broadcaster,
            notice.clone(),
        )
        .await
        .is_ok()
        {
            notified.push(recipient);
        }
        crate::backend::notifications::create::notify_subscribers(
            pool,
            broadcaster,
            notice,
            &notified,
        )
        .await;
    }
    let (fork_for_hook, source_for_hook) = (fork.clone(), source.clone());
    webhooks::emit(
        pool,
        &source,
        events::FORK,
        vec![actor],
        move |source_owner, people| {
            let forker = people.get(actor);
            payloads::fork(
                &fork_for_hook,
                &forker,
                &source_for_hook,
                source_owner,
                &fork_owner,
            )
        },
    )
    .await;
    Ok(fork)
}

async fn existing_fork(
    pool: &db::DbPool,
    actor: i64,
    owner: repo_lifecycle::RepositoryTarget,
    source: i64,
) -> Result<Option<Repository>, StorageError> {
    let Some(repo) = db::repositories::discovery::get_owner_fork(pool, owner, source)
        .await
        .map_err(|_| unavailable())?
    else {
        return Ok(None);
    };
    let repo = super::access::check_read(pool, repo.id, Some(actor))
        .await
        .map_err(|error| StorageError {
            kind: if error == super::access::AccessError::Unavailable {
                LifecycleFailure::Unavailable
            } else {
                LifecycleFailure::Denied
            },
            message: "Fork is unavailable.",
        })?;
    Ok(Some(repo))
}

pub struct TemplateOptions<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub private: bool,
    pub all_branches: bool,
    /// Organization property values chosen for the copy.
    pub properties: &'a [db::organizations::properties::ValueInput],
}

pub async fn generate(
    node: (&db::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: repo_lifecycle::RepositoryCreationOwner,
    source_id: i64,
    options: TemplateOptions<'_>,
) -> Result<Repository, StorageError> {
    let TemplateOptions {
        name,
        description,
        private,
        all_branches,
        properties,
    } = options;
    if let Some(description) = description {
        validate_description(description)?;
    }
    repo_lifecycle::check_creation_authority(node.0, owner)
        .await
        .map_err(storage_error)?;
    let source = super::access::check_read(node.0, source_id, Some(owner.actor_id))
        .await
        .map_err(|error| StorageError {
            kind: if error == super::access::AccessError::Unavailable {
                LifecycleFailure::Unavailable
            } else {
                LifecycleFailure::Denied
            },
            message: "Template repository is unavailable.",
        })?;
    if !source.is_template {
        return Err(StorageError {
            kind: LifecycleFailure::Invalid,
            message: "Source repository is not a template.",
        });
    }
    let refs = repo_store(&source, node.1)
        .refs_from_primary()
        .await
        .map_err(git_storage_error)?;
    if !refs.contains_key(&format!("refs/heads/{}", source.default_branch)) {
        return Err(StorageError {
            kind: LifecycleFailure::Invalid,
            message: "Template repository is empty.",
        });
    }
    // The copy runs in a task of its own. A request cancelled mid-copy then only drops the handle,
    // and the task still finishes the row or cleans it up, so no repository is left without storage.
    let pool = node.0.clone();
    let context = node.1.cloned();
    let repos_path = repos_path.to_owned();
    let name = name.to_owned();
    let description = description.map(str::to_owned);
    let properties = properties.to_vec();
    let task = tokio::spawn(async move {
        provision_from(
            (&pool, context.as_ref()),
            &repos_path,
            owner,
            &name,
            description.as_deref(),
            ProvisionOptions {
                private,
                creation_source: Some(CreationSource::Template {
                    repository: &source,
                    all_branches,
                }),
                gist: false,
            },
            &properties,
        )
        .await
    });
    task.await.map_err(|_| StorageError {
        kind: LifecycleFailure::Unavailable,
        message: "Repository creation did not finish.",
    })?
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
