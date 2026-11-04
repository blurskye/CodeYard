//! Creating repositories: the authorized database row, then its Git storage.
use chrono::Datelike;

use crate::cluster::context::NodeContext;
use crate::repositories::store::editor::UploadFile;
use crate::repositories::store::editor::UploadWrite;
use crate::repositories::store::repo_store;
use crate::repositories::templates::Template;
use chrono::Utc;
use codeyard_db_core::models::Repository;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
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

pub fn storage_error(error: anyhow::Error) -> StorageError {
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return StorageError {
            kind: LifecycleFailure::Denied,
            message: "A live unrestricted account credential is required.",
        };
    }
    if let Some(refusal) =
        error.downcast_ref::<codeyard_db_core::queries::organizations::properties::Refusal>()
    {
        return match refusal {
            codeyard_db_core::queries::organizations::properties::Refusal::Invalid {
                message,
                ..
            } => StorageError {
                kind: LifecycleFailure::Invalid,
                message,
            },
            codeyard_db_core::queries::organizations::properties::Refusal::Denied => StorageError {
                kind: LifecycleFailure::Denied,
                message: "Organization policy does not allow setting these repository properties.",
            },
            _ => StorageError {
                kind: LifecycleFailure::Invalid,
                message: "Invalid organization repository properties.",
            },
        };
    }
    match error
        .downcast_ref::<codeyard_db_repos::repositories::creation::RepositoryStorageRefusal>()
    {
        Some(codeyard_db_repos::repositories::creation::RepositoryStorageRefusal::Denied) => {
            StorageError {
                kind: LifecycleFailure::Denied,
                message: "You are not allowed to create this repository.",
            }
        }
        Some(
            codeyard_db_repos::repositories::creation::RepositoryStorageRefusal::StalePlacement,
        ) => unavailable(),
        None => {
            match error.downcast_ref::<codeyard_db_repos::repositories::records::RepositorySettingsRefusal>() {
                Some(
                    codeyard_db_repos::repositories::records::RepositorySettingsRefusal::Denied
                    | codeyard_db_repos::repositories::records::RepositorySettingsRefusal::VisibilityLocked,
                ) => StorageError {
                    kind: LifecycleFailure::Denied,
                    message: "Repository mutation is not permitted.",
                },
                Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::InvalidName) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "Invalid repository name.",
                    }
                }
                Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::NameConflict) => {
                    StorageError {
                        kind: LifecycleFailure::Conflict,
                        message: "A repository with this name already exists.",
                    }
                }
                Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::NoMergeMethod) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "At least one merge method must stay allowed.",
                    }
                }
                Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::InvalidTopics) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "Invalid repository topics.",
                    }
                }
                Some(codeyard_db_repos::repositories::records::RepositorySettingsRefusal::InvalidWebsite) => {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "Invalid repository website.",
                    }
                }
                Some(
                    codeyard_db_repos::repositories::records::RepositorySettingsRefusal::InternalNeedsOrganization,
                ) => StorageError {
                    kind: LifecycleFailure::Invalid,
                    message: "Only organization repositories can be internal.",
                },
                Some(
                    codeyard_db_repos::repositories::records::RepositorySettingsRefusal::InvalidSquashMessage,
                ) => StorageError {
                    kind: LifecycleFailure::Invalid,
                    message: "Invalid squash merge commit title or message.",
                },
                None if matches!(
                    error.downcast_ref::<codeyard_db_core::queries::secrets::SecretsError>(),
                    Some(
                        codeyard_db_core::queries::secrets::SecretsError::TotalTooLarge
                            | codeyard_db_core::queries::secrets::SecretsError::TooManySecrets
                    )
                ) =>
                {
                    StorageError {
                        kind: LifecycleFailure::Invalid,
                        message: "Delivered Actions secrets exceed the CI delivery budget.",
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
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || codeyard_support::fs::private_dir(&path))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
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
    pool: &codeyard_db_core::DbPool,
    context: Option<&NodeContext>,
    repository: &Repository,
) -> (bool, bool) {
    // Never remove storage while a live database row could still point at it.
    // Re-read the row because a delete may commit even when its response is lost.
    // A row that has accepted writes is kept, so a failed creation never discards a push.
    let deleted = if repository.is_gist {
        codeyard_db_repos::gists::discard(pool, repository.id, false)
            .await
            .and_then(|row| {
                row.ok_or_else(|| anyhow::anyhow!("Gist initialization is no longer discardable"))
            })
    } else {
        codeyard_db_repos::repositories::records::delete_unwritten_repository(pool, repository.id)
            .await
            .and_then(|row| {
                row.ok_or_else(|| anyhow::anyhow!("Repository has accepted writes and is kept"))
            })
    };
    let row_removed =
        match codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository.id)
            .await
        {
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
    pub owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
    pub properties: &'a [codeyard_db_core::queries::organizations::properties::ValueInput],
    pub admission: codeyard_db_repos::repositories::creation::CreationAdmission<'a>,
}

/// UUID directory names never change on a rename or ownership transfer.
pub async fn provision(
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
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
            admission: Default::default(),
        },
        name,
        description,
        private,
        source,
    )
    .await
}
pub async fn provision_with_properties(
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
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
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
    name: &str,
    source: Option<&Repository>,
) -> Result<Repository, StorageError> {
    provision_stored(
        node,
        repos_path,
        CreationInput {
            owner,
            properties: &[],
            admission: Default::default(),
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

#[expect(
    clippy::too_many_arguments,
    reason = "Provisioning takes the owner, the form's choices and the creating authority separately."
)]
async fn provision_from(
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
    name: &str,
    description: Option<&str>,
    options: ProvisionOptions<'_>,
    chosen: &[codeyard_db_core::queries::organizations::properties::ValueInput],
    authority: Option<&codeyard_db_core::queries::credentials::Actor>,
) -> Result<Repository, StorageError> {
    let mut properties = inherited_properties(node.0, owner, options.creation_source).await?;
    // Values chosen in the creation form replace inherited ones.
    properties.retain(|value| {
        !chosen.iter().any(|chosen| {
            chosen
                .property_name
                .eq_ignore_ascii_case(&value.property_name)
        })
    });
    properties.extend_from_slice(chosen);
    provision_stored(
        node,
        repos_path,
        CreationInput {
            owner,
            properties: &properties,
            admission: codeyard_db_repos::repositories::creation::CreationAdmission {
                authority,
                advisory: None,
            },
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
    pool: &codeyard_db_core::DbPool,
    owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
    source: Option<CreationSource<'_>>,
) -> Result<Vec<codeyard_db_core::queries::organizations::properties::ValueInput>, StorageError> {
    let (
        Some(source),
        codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(org),
    ) = (source.map(CreationSource::repository), owner.target)
    else {
        return Ok(Vec::new());
    };
    if source.owner_org_id != Some(org) {
        return Ok(Vec::new());
    }
    let Some(codeyard_db_orgs::organizations::properties::RepositoryMetadata {
        values,
        definitions,
        organization_owner,
        ..
    }) = codeyard_db_orgs::organizations::properties::repository(
        pool,
        Some(owner.actor_id),
        source.id,
    )
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
                                == codeyard_db_core::queries::organizations::properties::EditableBy::OrgAndRepoActors)
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
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    creation: CreationInput<'_>,
    name: &str,
    description: Option<&str>,
    options: ProvisionOptions<'_>,
) -> Result<Repository, StorageError> {
    let (pool, context, repos_path, name, description, properties) = (
        node.0.clone(),
        node.1.cloned(),
        repos_path.to_owned(),
        name.to_owned(),
        description.map(str::to_owned),
        creation.properties.to_vec(),
    );
    let owner = creation.owner;
    let authority = creation.admission.authority.cloned();
    let advisory = creation.admission.advisory.map(str::to_owned);
    let (private, gist) = (options.private, options.gist);
    let source = options
        .creation_source
        .map(|source| source.repository().clone());
    let template = matches!(
        options.creation_source,
        Some(CreationSource::Template { .. })
    );
    let branch_option = match options.creation_source {
        Some(CreationSource::Fork {
            default_branch_only,
            ..
        }) => default_branch_only,
        Some(CreationSource::Template { all_branches, .. }) => all_branches,
        None => false,
    };
    // Once the row is inserted, initialization or conservative cleanup must outlive the
    // caller. Otherwise cancellation leaves a discoverable but unusable reservation. The task
    // keeps the request's span and log subscriber, so its errors are logged with the request.
    tokio::spawn(
        async move {
            let creation_source = source.as_ref().map(|repository| {
                if template {
                    CreationSource::Template {
                        repository,
                        all_branches: branch_option,
                    }
                } else {
                    CreationSource::Fork {
                        repository,
                        default_branch_only: branch_option,
                    }
                }
            });
            provision_stored_owned(
                (&pool, context.as_ref()),
                &repos_path,
                CreationInput {
                    owner,
                    properties: &properties,
                    admission: codeyard_db_repos::repositories::creation::CreationAdmission {
                        authority: authority.as_ref(),
                        advisory: advisory.as_deref(),
                    },
                },
                &name,
                description.as_deref(),
                ProvisionOptions {
                    private,
                    creation_source,
                    gist,
                },
            )
            .await
        }
        .in_current_span()
        .with_current_subscriber(),
    )
    .await
    .map_err(|_| unavailable())?
}

async fn provision_stored_owned(
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
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
    let storage_node_id = crate::cluster::placement::storage_node(context, source)
        .await
        .map_err(git_storage_error)?;
    if storage_node_id.is_some()
        && codeyard_db_cluster::cluster::writes::write_settings(pool)
            .await
            .map_err(|_| unavailable())?
            .write_quorum
            .is_some()
    {
        // The ref journal requires an existing serving term on both voters.
        return Err(StorageError {
            kind: LifecycleFailure::Unavailable,
            message: "Repository initialization and copy are unavailable with write quorum.",
        });
    }
    let root = std::path::absolute(repos_path).map_err(|_| unavailable())?;
    let location = crate::repositories::store::primary_path(&root, Uuid::new_v4());
    let disk_path = location.to_str().ok_or_else(unavailable)?;
    let input = codeyard_db_repos::repositories::creation::NewRepository {
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
        codeyard_db_repos::repositories::creation::insert_gist_repository(pool, input).await
    } else {
        codeyard_db_repos::repositories::creation::insert_repository_with_admission(
            pool,
            input,
            creation.properties,
            creation.admission,
        )
        .await
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
                codeyard_db_repos::repositories::records::update_repo_default_branch(
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
                    creation
                        .admission
                        .authority
                        .cloned()
                        .unwrap_or_else(|| owner.actor_id.into()),
                    &repo_store(source, context),
                    &crate::repositories::store::CopyOptions {
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
            codeyard_db_cluster::cluster::bump_write_version(pool, repository.id, None)
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
        let accounting = store.refresh_storage_size_now(pool, repository.id).await;
        match accounting {
            Ok(size) => repository.size_bytes = size,
            Err(error) => {
                tracing::warn!(repo_id = repository.id, %error, "fork created but size accounting failed")
            }
        }
    }
    if source.is_none() || template {
        let created = repository.clone();
        let actor = owner.actor_id;
        crate::webhooks::emit(
            pool,
            &repository,
            codeyard_model::webhook_events::REPOSITORY,
            vec![actor],
            move |owner, people| {
                serde_json::json!({
                    "action": "created",
                    "repository": crate::webhooks::payloads::repository(&created, owner),
                    "sender": crate::webhooks::payloads::person(&people.get(actor)),
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
    pub fn is_empty(&self) -> bool {
        !self.readme && self.gitignore.is_none() && self.license.is_none()
    }
}

/// Creates a repository as [`provision`] does and, when `files` asks for any, commits them as
/// the first commit on the default branch. Once storage is published, later failures leave it intact.
pub async fn provision_with_initial_files(
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
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
            admission: Default::default(),
        },
        name,
        description,
        private,
        files,
    )
    .await
}
pub async fn provision_with_initial_properties(
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    input: CreationInput<'_>,
    name: &str,
    description: Option<&str>,
    private: bool,
    files: InitialFiles,
) -> Result<Repository, StorageError> {
    let authority = input
        .admission
        .authority
        .cloned()
        .unwrap_or_else(|| input.owner.actor_id.into());
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
        authority,
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
    pool: &codeyard_db_core::DbPool,
    context: Option<&NodeContext>,
    authority: codeyard_db_core::queries::credentials::Actor,
    repository: &Repository,
    description: Option<&str>,
    files: InitialFiles,
) -> Result<(), StorageError> {
    let actor = authority.user_id;
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor)
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
            crate::repositories::templates::readme(&repository.name, description),
        ));
    }
    if let Some(template) = files.gitignore {
        texts.push((".gitignore", template.text.to_owned()));
    }
    if let Some(template) = files.license {
        texts.push((
            "LICENSE",
            crate::repositories::templates::fill_license(template.text, year, holder),
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
    let mut prepared = store
        .prepare_upload(&write, &content, &user.username, &user.commit_email())
        .await
        .map_err(git_storage_error)?;
    prepared.authority = Some(authority);
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
    pool: &codeyard_db_core::DbPool,
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
    pool: &codeyard_db_core::DbPool,
    repository: &Repository,
) -> Result<bool, StorageError> {
    codeyard_db_repos::repositories::creation::repository_visibility_locked(pool, repository.id)
        .await
        .map_err(|_| unavailable())
}

pub fn validate_name(name: &str) -> Result<(), StorageError> {
    if !codeyard_support::repository_names::valid(name) {
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
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
) -> Result<(), StorageError> {
    let actor = actor.into();
    codeyard_identity::repository_access::check_admin(pool, repo_id, actor.user_id)
        .await
        .map_err(|error| StorageError {
            kind: if error == codeyard_identity::repository_access::AccessError::Unavailable {
                LifecycleFailure::Unavailable
            } else {
                LifecycleFailure::Denied
            },
            message: "Repository deletion is not available.",
        })?;
    // Remove files where the deleted row says they are. A promotion may have moved the
    // primary after the check above.
    let deleted = codeyard_db_repos::repositories::records::delete_repository(pool, actor, repo_id)
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
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    new_owner: &str,
) -> Result<Repository, StorageError> {
    let repository = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo_id)
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
    pool: &codeyard_db_core::DbPool,
    handle: &str,
) -> Result<
    (
        codeyard_db_repos::repositories::creation::RepositoryTarget,
        String,
    ),
    StorageError,
> {
    if let Some(user) = codeyard_db_identity::users::accounts::get_user_by_username(pool, handle)
        .await
        .map_err(|_| unavailable())?
    {
        return Ok((
            codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(user.id),
            user.username,
        ));
    }
    if let Some(org) = super::get_public_organization(pool, handle)
        .await
        .map_err(|_| unavailable())?
    {
        return Ok((
            codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(org.id),
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
    pool: &codeyard_db_core::DbPool,
    context: Option<&NodeContext>,
    repos_path: &Path,
    broadcaster: &codeyard_notifications::broadcast::NotificationBroadcaster,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    source_id: i64,
    owner: &str,
) -> Result<Repository, StorageError> {
    let authority = actor.into();
    let actor = authority.user_id;
    fork_into(
        pool,
        context,
        repos_path,
        broadcaster,
        ForkOptions {
            owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(
                actor,
            ),
            name: None,
            default_branch_only: false,
            properties: &[],
            authority: Some(&authority),
        },
        source_id,
        owner,
    )
    .await
}

pub struct ForkOptions<'a> {
    pub owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
    pub name: Option<&'a str>,
    pub default_branch_only: bool,
    /// Organization property values chosen for the copy.
    pub properties: &'a [codeyard_db_core::queries::organizations::properties::ValueInput],
    pub authority: Option<&'a codeyard_db_core::queries::credentials::Actor>,
}

pub async fn fork_into(
    pool: &codeyard_db_core::DbPool,
    context: Option<&NodeContext>,
    repos_path: &Path,
    broadcaster: &codeyard_notifications::broadcast::NotificationBroadcaster,
    options: ForkOptions<'_>,
    source_id: i64,
    owner: &str,
) -> Result<Repository, StorageError> {
    let actor = options.owner.actor_id;
    live_creation_actor(pool, actor, options.authority).await?;
    codeyard_db_repos::repositories::creation::check_creation_authority(pool, options.owner)
        .await
        .map_err(storage_error)?;
    let source = codeyard_identity::repository_access::check_read(pool, source_id, Some(actor))
        .await
        .map_err(|error| StorageError {
            kind: if error == codeyard_identity::repository_access::AccessError::Unavailable {
                LifecycleFailure::Unavailable
            } else {
                LifecycleFailure::Denied
            },
            message: "Repository is not available for forking.",
        })?;
    // As on GitHub, the switch only governs forks of private and internal repositories.
    if source.is_private
        && !codeyard_db_repos::repositories::records::repository_features(pool, source.id)
            .await
            .map_err(|_| unavailable())?
            .allow_forking
    {
        return Err(StorageError {
            kind: LifecycleFailure::Denied,
            message: "Forking is disabled for this repository.",
        });
    }
    let same_owner = match options.owner.target {
        codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(id) => {
            source.is_personal_owner(id)
        }
        codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(id) => {
            source.owner_org_id == Some(id)
        }
    };
    if same_owner {
        return Err(StorageError {
            kind: LifecycleFailure::OwnRepositoryFork,
            message: "Cannot fork into the source owner.",
        });
    }
    if let Some(existing) = existing_fork(pool, actor, options.owner.target, source.id).await? {
        live_creation_actor(pool, actor, options.authority).await?;
        return Ok(existing);
    }
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    let fork_owner = match options.owner.target {
        codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(_) => {
            user.username.clone()
        }
        codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(id) => {
            let codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) =
                super::get_organization_metadata(pool, actor, id)
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
        options.authority,
    )
    .await
    {
        Ok(fork) => fork,
        Err(error) if error.kind == LifecycleFailure::Conflict => {
            if let Some(existing) =
                existing_fork(pool, actor, options.owner.target, source.id).await?
            {
                live_creation_actor(pool, actor, options.authority).await?;
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
        let notice = codeyard_notifications::create::NotificationRequest {
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
        // The source owner may not be able to read the fork's destination.
        if codeyard_identity::repository_access::check_read(pool, fork.id, Some(recipient))
            .await
            .is_ok()
            && codeyard_notifications::create::create_and_broadcast_notification(
                pool,
                broadcaster,
                notice.clone(),
            )
            .await
            .is_ok()
        {
            notified.push(recipient);
        }
        codeyard_notifications::create::notify_subscribers(pool, broadcaster, notice, &notified)
            .await;
    }
    // Even a redacted source-scoped event reveals confidential remediation activity. A
    // source subscription cannot establish readability of a private destination.
    if fork.is_private {
        return Ok(fork);
    }
    let public =
        match codeyard_db_repos::repositories::records::get_repository_by_id(pool, fork.id).await {
            Ok(Some(current)) => !current.is_private,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(repo_id = fork.id, %error, "fork webhook visibility unavailable");
                false
            }
        };
    if !public {
        return Ok(fork);
    }
    let (fork_for_hook, source_for_hook) = (fork.clone(), source.clone());
    crate::webhooks::emit(
        pool,
        &source,
        codeyard_model::webhook_events::FORK,
        vec![actor],
        move |source_owner, people| {
            let forker = people.get(actor);
            crate::webhooks::payloads::fork(
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

async fn live_creation_actor(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    authority: Option<&codeyard_db_core::queries::credentials::Actor>,
) -> Result<(), StorageError> {
    if let Some(authority) = authority {
        if authority.user_id != actor {
            return Err(storage_error(
                codeyard_support::credentials::AccountRestricted.into(),
            ));
        }
        codeyard_identity::repository_access::credential_user(pool, authority, true)
            .await
            .map_err(storage_error)?;
    }
    Ok(())
}

async fn existing_fork(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    owner: codeyard_db_repos::repositories::creation::RepositoryTarget,
    source: i64,
) -> Result<Option<Repository>, StorageError> {
    let Some(repo) =
        codeyard_db_repos::repositories::discovery::get_owner_fork(pool, owner, source)
            .await
            .map_err(|_| unavailable())?
    else {
        return Ok(None);
    };
    let repo = codeyard_identity::repository_access::check_read(pool, repo.id, Some(actor))
        .await
        .map_err(|error| StorageError {
            kind: if error == codeyard_identity::repository_access::AccessError::Unavailable {
                LifecycleFailure::Unavailable
            } else {
                LifecycleFailure::Denied
            },
            message: "Fork is unavailable.",
        })?;
    if !codeyard_db_identity::interactions::source(pool, repo.id)
        .await
        .map_err(|_| unavailable())?
        .is_some_and(|source| source.write_version > 0)
    {
        return Err(StorageError {
            kind: LifecycleFailure::Conflict,
            message: "The existing fork is still being initialized. Try again shortly.",
        });
    }
    Ok(Some(repo))
}

pub struct TemplateOptions<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub private: bool,
    pub all_branches: bool,
    /// Organization property values chosen for the copy.
    pub properties: &'a [codeyard_db_core::queries::organizations::properties::ValueInput],
    pub authority: Option<&'a codeyard_db_core::queries::credentials::Actor>,
}

pub async fn generate(
    node: (&codeyard_db_core::DbPool, Option<&NodeContext>),
    repos_path: &Path,
    owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
    source_id: i64,
    options: TemplateOptions<'_>,
) -> Result<Repository, StorageError> {
    let TemplateOptions {
        name,
        description,
        private,
        all_branches,
        properties,
        authority,
    } = options;
    if let Some(description) = description {
        validate_description(description)?;
    }
    live_creation_actor(node.0, owner.actor_id, authority).await?;
    codeyard_db_repos::repositories::creation::check_creation_authority(node.0, owner)
        .await
        .map_err(storage_error)?;
    let source =
        codeyard_identity::repository_access::check_read(node.0, source_id, Some(owner.actor_id))
            .await
            .map_err(|error| StorageError {
                kind: if error == codeyard_identity::repository_access::AccessError::Unavailable {
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
    let authority = authority.cloned();
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
            authority.as_ref(),
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
