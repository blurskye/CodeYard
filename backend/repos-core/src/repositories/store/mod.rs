//! The one way to reach a repository's Git data.
//!
//! Callers get a [`RepoStore`] from [`repo_store`] and ask it for what they need.
//! Nothing outside this module opens a repository directory or runs `git` against
//! one. Local stores use this node's disk; remote stores forward the same calls
//! over iroh to the owning storage node. Inputs and outputs are owned values.
//! The one exception is [`RepoStore::local_path_for_spawning_git`], used by the
//! host OpenSSH gateway.
use crate::context::NodeContext;
use crate::store_types::AsyncMergePublication;
use crate::store_types::BlobBytes;
use crate::store_types::BranchMergePublication;
use crate::store_types::GitService;
use crate::store_types::GitSession;
use crate::store_types::GitStreams;
use crate::store_types::MergeMethod;
use crate::store_types::MergeRequest;
use crate::store_types::PullBranchUpdate;
use crate::store_types::PullHeadActor;
use crate::store_types::RestackMove;
use crate::store_types::RevertRequest;
use crate::store_types::UpstreamMerge;
use crate::store_types::MAX_ARCHIVE_RESPONSE_BYTES;
use crate::store_types::MAX_COMMIT_OFFSET;
use crate::store_types::MAX_COMMIT_PAGE_SIZE;
use crate::wire::Operation;
use codeyard_db_core::models::Repository;
use codeyard_git as git;
use codeyard_git::error::invalid_ref;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::gist_types::GistEdit;
use codeyard_git::gist_types::GistRevision;
use codeyard_git::gist_types::GistSnapshot;
use codeyard_git::repository::process;
use codeyard_git::BlobContent;
use codeyard_git::BranchStatus;
use codeyard_git::CiCommit;
use codeyard_git::CiWorkflowFile;
use codeyard_git::CiWorkflowSource;
use codeyard_git::CommitComparison;
use codeyard_git::CommitFilter;
use codeyard_git::CommitInfo;
use codeyard_git::ContributorInfo;
use codeyard_git::DiffInfo;
use codeyard_git::FileBlame;
use codeyard_git::FileList;
use codeyard_git::LanguageStat;
use codeyard_git::RepoInfo;
use codeyard_git::TagInfo;
use codeyard_git::TreeEntry;
use codeyard_git::WeekChanges;
use codeyard_repos::editor::CommitIdentities;
use codeyard_repos::editor::FileWrite;
use codeyard_repos::editor::PreparedFile;
use codeyard_repos::editor::UploadWrite;
use codeyard_repos::gists::GistCredential;
use codeyard_repos::protection::WriteVia;
use codeyard_storage::range::ByteRange as BlobRange;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

pub mod local;
mod remote;

pub fn primary_path(root: &Path, uuid: uuid::Uuid) -> std::path::PathBuf {
    root.join(format!("{uuid}.git"))
}

/// The store for a repository row. Cheap to create and to clone.
pub fn repo_store(repo: &Repository, context: Option<&NodeContext>) -> RepoStore {
    // A stub is identity metadata and never names this cell's Git storage.
    if repo.is_stub {
        return RepoStore(Backend::Remote(remote::RemoteStore::unavailable()));
    }
    match repo.storage_node_id {
        None => RepoStore(Backend::Local(
            crate::repositories::store::local::LocalStore::new(&repo.disk_path, Some(None)),
        )),
        Some(id) if context.is_some_and(|context| context.node_id == id) => {
            let context = context.expect("this node owns the repository");
            match crate::wire::repository_uuid(&repo.disk_path) {
                Ok(uuid) => RepoStore(Backend::Local(
                    crate::repositories::store::local::LocalStore::new(
                        &primary_path(&context.repos_path, uuid).to_string_lossy(),
                        Some(Some(id)),
                    )
                    .with_context(context, repo.id),
                )),
                Err(_) => RepoStore(Backend::Remote(
                    crate::repositories::store::remote::RemoteStore::new(&repo.disk_path, id, None),
                )),
            }
        }
        Some(id) => RepoStore(Backend::Remote(
            crate::repositories::store::remote::RemoteStore::new(&repo.disk_path, id, context),
        )),
    }
}

impl RepoStore {
    pub async fn recover_term(
        pool: &codeyard_db_core::DbPool,
        repository: i64,
        node: i64,
        root: &Path,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(
            process::RPC_TIMEOUT,
            crate::repositories::store::local::recover_term(pool, repository, node, root),
        )
        .await?
    }

    /// Storage RPC admission keeps the database and captured term with the local store.
    pub(crate) async fn on_context(
        disk_path: &str,
        repository: i64,
        context: &NodeContext,
    ) -> Result<Self, RepoError> {
        let local = crate::repositories::store::local::LocalStore::new(
            disk_path,
            Some(Some(context.node_id)),
        )
        .with_context(context, repository);
        if local.admitted_path(true).await? != Path::new(disk_path) {
            return Err(RepoError::storage_node_unavailable());
        }
        Ok(Self(Backend::Local(local)))
    }

    /// A reserved repository has no serving term yet. Only initialization may use this store.
    pub fn for_initialization(
        repository: &Repository,
        context: &NodeContext,
    ) -> Result<Self, RepoError> {
        if repository.is_stub {
            return Err(crate::wire::unavailable());
        }
        let uuid = crate::wire::repository_uuid(&repository.disk_path)?;
        Ok(Self(Backend::Local(
            local::LocalStore::new(
                primary_path(&context.repos_path, uuid)
                    .to_str()
                    .ok_or_else(crate::wire::unavailable)?,
                Some(Some(context.node_id)),
            )
            .with_context(context, repository.id),
        )))
    }

    /// CI workers receive a storage node and UUID, not a repository database row.
    pub fn remote(uuid: uuid::Uuid, node_id: i64, context: &NodeContext) -> Self {
        Self(Backend::Remote(
            crate::repositories::store::remote::RemoteStore::new(
                &format!("{uuid}.git"),
                node_id,
                Some(context),
            ),
        ))
    }

    pub fn for_ci_run(mut self, id: i64, worker: &str) -> Self {
        if let Backend::Remote(remote) = &mut self.0 {
            remote.for_ci_run(id, worker);
        }
        self
    }
}

/// A repository's Git data, wherever it lives.
#[derive(Clone, Debug)]
pub struct RepoStore(Backend);

pub struct CopyOptions {
    pub default_branch: String,
    pub template: bool,
    pub default_branch_only: bool,
}

#[derive(Clone, Debug)]
enum Backend {
    Local(crate::repositories::store::local::LocalStore),
    Remote(crate::repositories::store::remote::RemoteStore),
}

impl RepoStore {
    pub fn local(&self) -> Result<&crate::repositories::store::local::LocalStore, RepoError> {
        match &self.0 {
            Backend::Local(local) => Ok(local),
            Backend::Remote(_) => Err(crate::wire::invalid(
                "fork source is not on the destination node",
            )),
        }
    }

    /// Reads a tree object, optionally including its descendants, with bounded output.
    pub async fn git_tree(
        &self,
        revision: &str,
        recursive: bool,
    ) -> Result<git::objects::TreeObject, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.git_tree(revision, recursive).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::GitTree {
                            revision: revision.into(),
                            recursive,
                            raw_json: true,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    pub async fn introduced_blobs(
        &self,
        sha: &str,
    ) -> Result<Vec<git::objects::TreeObjectEntry>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.introduced_blobs(sha).await,
            Backend::Remote(remote) => {
                remote
                    .call(Operation::IntroducedBlobs { sha: sha.into() }, b"")
                    .await
            }
        }
    }

    pub async fn git_object_kind(&self, sha: &str) -> Result<String, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.git_object_kind(sha).await,
            Backend::Remote(remote) => {
                remote
                    .call(Operation::GitObjectKind { sha: sha.into() }, b"")
                    .await
            }
        }
    }

    pub async fn git_tag(&self, sha: &str) -> Result<git::objects::TagObject, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.git_tag(sha).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::GitTag {
                            sha: sha.into(),
                            raw_json: true,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    pub async fn update_reference_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        reference: &str,
        sha: &str,
        force: bool,
    ) -> Result<(String, String), RepoError> {
        let actor = actor.into();
        match &self.0 {
            Backend::Local(local) => {
                local
                    .update_reference(pool, repo_id, actor, reference, sha, force)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor.user_id,
                        Operation::UpdateReference {
                            reference: reference.into(),
                            sha: crate::wire::PublicationText::retain(sha, actor),
                            force,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Writes an unattached Git object after checking live write authority on its storage node.
    pub async fn write_git_object_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        token: Option<i64>,
        input: git::objects::ObjectWrite,
        bytes: &[u8],
    ) -> Result<String, RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .write_git_object(pool, repo_id, actor, token, input, bytes)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(actor, Operation::WriteGitObject { input, token }, bytes)
                    .await
            }
        }
    }

    pub async fn lock_migration_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        actor: i64,
        migration: i64,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.lock_migration(pool, repo, actor, migration).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(actor, Operation::LockMigration { migration }, b"")
                    .await
            }
        }
    }

    pub async fn gist_snapshot(
        &self,
        pool: &codeyard_db_core::DbPool,
        id: i64,
        revision: Option<&str>,
    ) -> Result<GistSnapshot, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.gist_snapshot(pool, id, revision).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::GistSnapshot {
                            revision: revision.map(str::to_owned),
                        },
                        b"",
                    )
                    .await
            }
        }
    }
    pub async fn gist_history(
        &self,
        pool: &codeyard_db_core::DbPool,
        id: i64,
        revision: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<GistRevision>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.gist_history(pool, id, revision, offset, limit).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::GistHistory {
                            revision: revision.map(str::to_owned),
                            offset,
                            limit,
                        },
                        b"",
                    )
                    .await
            }
        }
    }
    pub async fn gist_write(
        &self,
        pool: &codeyard_db_core::DbPool,
        id: i64,
        actor: i64,
        credential: GistCredential,
        input: GistEdit,
    ) -> Result<GistSnapshot, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.gist_write(pool, id, actor, credential, input).await,
            Backend::Remote(remote) => {
                let bytes = serde_json::to_vec(&input)
                    .map_err(|_| RepoError::new(ErrorKind::Invalid, "Invalid gist input"))?;
                remote
                    .call_as(actor, Operation::GistWrite { credential }, &bytes)
                    .await
            }
        }
    }

    pub async fn migration_archive_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        actor: i64,
    ) -> Result<Vec<u8>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.migration_archive(pool, repo, actor).await,
            Backend::Remote(remote) => remote.bytes_as(actor, Operation::MigrationArchive).await,
        }
    }

    pub async fn import_git_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        import: codeyard_repos::imports::GitImport,
        pack: Vec<u8>,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                let (local, pool) = (local.clone(), pool.clone());
                tokio::spawn(
                    async move { local.import_git(&pool, repo_id, actor, import, &pack).await },
                )
                .await
                .map_err(|_| RepoError::applied_followup_failed())?
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(actor, Operation::ImportGit { import }, &pack)
                    .await
            }
        }
    }

    pub async fn cancel_import_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.cancel_import(pool, repo_id, actor).await,
            Backend::Remote(remote) => remote.call_as(actor, Operation::CancelImport, b"").await,
        }
    }

    // ----- Opening and lifecycle -----

    /// The store for a stored `disk_path` value, for when no `Repository` is at
    /// hand: removing storage after its row is gone, tools and tests. Application
    /// code uses [`repo_store`].
    pub fn from_disk_path(disk_path: &str) -> RepoStore {
        RepoStore(Backend::Local(
            crate::repositories::store::local::LocalStore::new(disk_path, None),
        ))
    }

    /// A repository directory on storage node `node_id`. Its writes stop once the
    /// repository has been promoted to another node.
    pub fn on_node(disk_path: &str, node_id: i64) -> RepoStore {
        RepoStore(Backend::Local(
            crate::repositories::store::local::LocalStore::new(disk_path, Some(Some(node_id))),
        ))
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
        let local = crate::repositories::store::local::LocalStore::create(
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

    /// Copies privately, then checks live source read and destination write authority before publication.
    pub async fn initialize_copy(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        source_repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        source: &RepoStore,
        options: &CopyOptions,
    ) -> Result<(), RepoError> {
        let authority = actor.into();
        let actor = authority.user_id;
        match (&self.0, &source.0) {
            (Backend::Local(local), Backend::Local(from)) => {
                local
                    .initialize_copy(pool, repo_id, source_repo_id, authority, from, options)
                    .await
            }
            (Backend::Remote(remote), Backend::Remote(from)) if remote.node_id == from.node_id => {
                let source = crate::wire::repository_uuid(&from.disk_path)?;
                let operation = Operation::InitializeCopy {
                    source,
                    default_branch: crate::wire::PublicationText::retain(
                        &options.default_branch,
                        authority,
                    ),
                    template: options.template,
                    default_branch_only: options.default_branch_only,
                };
                remote.call_as(actor, operation, b"").await
            }
            _ => Err(crate::wire::invalid(
                "source is not on the destination node",
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

    /// Removes a deleted repository. A local repository moves to `deleted/` under its
    /// parent directory and stays there for [`codeyard_db_core::queries::files::RESTORE_WINDOW_SECONDS`], so a database
    /// restored to before the delete still finds its files. Remote storage uses the same retirement.
    pub async fn retire(&self) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.retire().await,
            Backend::Remote(remote) => remote.call(Operation::Retire, b"").await,
        }
    }

    /// Whether the storage holds a Git repository at all.
    pub async fn exists(&self) -> bool {
        match &self.0 {
            Backend::Local(local) => local.exists().await,
            Backend::Remote(remote) => remote.call(Operation::Exists, b"").await.unwrap_or(false),
        }
    }

    /// Accepted primary writes share one coalesced accounting owner with file mutations.
    pub(crate) fn refresh_storage_size(&self, pool: codeyard_db_core::DbPool, repo_id: i64) {
        if let Backend::Local(store) = &self.0 {
            crate::repositories::store::local::refresh_storage_size(pool, repo_id, store.clone());
        }
    }

    /// Wait for this request's coalesced scan to be published, including on a remote primary.
    pub async fn refresh_storage_size_now(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
    ) -> Result<i64, RepoError> {
        match &self.0 {
            Backend::Local(store) => {
                crate::repositories::store::local::refresh_storage_size_now(
                    pool.clone(),
                    repo_id,
                    store.clone(),
                )
                .await
            }
            Backend::Remote(remote) => remote.call(Operation::RefreshStorageSize, b"").await,
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

    /// Public branches, tags and pull refs whose full names match this prefix.
    pub async fn matching_refs(&self, prefix: &str) -> Result<HashMap<String, String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.matching_refs(prefix).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::MatchingRefs {
                            prefix: prefix.to_owned(),
                        },
                        b"",
                    )
                    .await
            }
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

    /// Primary-only server provenance, never inferred from commit authors or cached receipts.
    pub async fn branch_publication(
        &self,
        repository_id: i64,
        reference: &str,
    ) -> Result<Option<git::PublicationRecord>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.branch_publication(repository_id, reference).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::BranchPublication {
                            repository_id,
                            reference: reference.into(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    pub async fn existing_commits(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashSet<String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.existing_commits(ids).await,
            Backend::Remote(_) => Err(crate::wire::invalid(
                "replica commit checks run on their node",
            )),
        }
    }

    /// Every ref the repository stores, including pull request heads, for a replica. Only a
    /// repository on this node has them.
    pub async fn mirror_refs(&self) -> Result<HashMap<String, String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.mirror_refs().await,
            Backend::Remote(_) => Err(crate::wire::invalid(
                "replica refs are listed on their node",
            )),
        }
    }

    /// The refs and HEAD a replica copy takes, as one listing. Refused while a ref change to
    /// the repository is in progress here, so a copy never claims a version it lacks.
    pub async fn replica_listing(
        &self,
    ) -> Result<(HashMap<String, String>, Option<String>), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.replica_listing().await,
            Backend::Remote(_) => Err(crate::wire::invalid(
                "replica refs are listed on their node",
            )),
        }
    }

    /// All refs and the default branch. A local repository reads the default from HEAD, so
    /// the branches are listed once. A remote one asks for both at the same time.
    pub async fn refs_and_default(&self) -> Result<(HashMap<String, String>, String), RepoError> {
        match &self.0 {
            Backend::Local(local) => Ok((local.refs().await?, local.default_branch().await?)),
            Backend::Remote(_) => {
                let (refs, info) = tokio::try_join!(self.refs(), self.info())?;
                Ok((refs, info.default_branch))
            }
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
        self.list_tree(git_ref, path, false).await
    }

    /// Directory names, types and sizes without file history.
    pub async fn tree_metadata(
        &self,
        git_ref: &str,
        path: &str,
    ) -> Result<Vec<TreeEntry>, RepoError> {
        self.list_tree(git_ref, path, true).await
    }

    async fn list_tree(
        &self,
        git_ref: &str,
        path: &str,
        metadata_only: bool,
    ) -> Result<Vec<TreeEntry>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.tree(git_ref, path, metadata_only).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::Tree {
                            git_ref: git_ref.into(),
                            path: path.into(),
                            metadata_only,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// History-free, aggregate-bounded sidebar folders, fetched in one remote call.
    pub async fn sidebar_trees(
        &self,
        git_ref: &str,
        paths: &[String],
    ) -> Result<Vec<(String, Vec<TreeEntry>)>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.sidebar_trees(git_ref, paths).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::SidebarTrees {
                            git_ref: git_ref.into(),
                            paths: paths.to_vec(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Every file path at `git_ref`, sorted. The listing keeps at most `limit` paths.
    pub async fn file_paths(&self, git_ref: &str, limit: usize) -> Result<FileList, RepoError> {
        self.file_paths_matching(git_ref, "", limit).await
    }

    /// Match across the entire tree before retaining the bounded sorted result.
    pub async fn file_paths_matching(
        &self,
        git_ref: &str,
        query: &str,
        limit: usize,
    ) -> Result<FileList, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.file_paths(git_ref, limit, query).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::FilePaths {
                            git_ref: if query.is_empty() {
                                git_ref.into()
                            } else {
                                crate::wire::FilePathQuery::Filtered {
                                    reference: git_ref.into(),
                                    query: query.into(),
                                }
                            },
                            limit,
                            raw_json: true,
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

    /// Bounded template texts at one revision, in input order. Non-files are absent.
    pub async fn template_texts(
        &self,
        revision: &str,
        paths: &[String],
    ) -> Result<Vec<Option<(String, String)>>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.template_texts(revision, paths).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::TemplateTexts {
                            revision: revision.into(),
                            paths: paths.to_vec(),
                            raw_json: true,
                        },
                        b"",
                    )
                    .await
            }
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

    /// Bounded bytes and metadata of a blob addressed by its immutable ID.
    pub async fn blob_by_id_range(
        &self,
        id: &str,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.blob_by_id_range(id, range).await,
            Backend::Remote(remote) => remote.blob_by_id_range(id, range).await,
        }
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
                        git::objects::MAX_BLOB_WRITE_BYTES,
                    )
                    .await
            }
        }
    }

    /// Up to `limit` commits after `offset` that pass `filter`, starting at `git_ref` and visiting
    /// the newest pending ancestor first. Unvisited history is never loaded.
    pub async fn commits(
        &self,
        git_ref: &str,
        offset: usize,
        limit: usize,
        filter: &CommitFilter,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        let deadline = Instant::now() + git::COMMIT_WALK_TIMEOUT;
        self.commits_until(git_ref, offset, limit, filter, deadline)
            .await
    }

    /// `commits`, with the same remaining walk budget on local and remote storage.
    pub async fn commits_until(
        &self,
        git_ref: &str,
        offset: usize,
        limit: usize,
        filter: &CommitFilter,
        deadline: Instant,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        if offset > MAX_COMMIT_OFFSET || limit > MAX_COMMIT_PAGE_SIZE {
            return Err(RepoError::response_too_large());
        }
        match &self.0 {
            Backend::Local(local) => {
                local
                    .commits_until(git_ref, offset, limit, filter, deadline)
                    .await
            }
            Backend::Remote(remote) => {
                let walk_millis = Some(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                );
                let operation = if offset == 0 {
                    Operation::Commits {
                        git_ref: git_ref.into(),
                        limit,
                        filter: filter.clone(),
                        walk_millis,
                        raw_json: true,
                    }
                } else {
                    Operation::CommitPage {
                        git_ref: git_ref.into(),
                        offset,
                        limit,
                        filter: filter.clone(),
                        walk_millis,
                        raw_json: true,
                    }
                };
                remote.call(operation, b"").await
            }
        }
    }

    /// Lines of the files at `git_ref` that contain `text`, from the `offset`th on.
    pub async fn search_code(
        &self,
        git_ref: &str,
        text: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<git::CodeMatch>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.search_code(git_ref, text, offset, limit).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::SearchCode {
                            git_ref: git_ref.into(),
                            text: text.into(),
                            offset,
                            limit,
                            raw_json: true,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Full count and the requested offset/limit window within the oldest 250 commits. Limit zero counts only.
    pub async fn pull_request_commits(
        &self,
        base_ref: &str,
        head_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(u64, Vec<CommitInfo>), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .pull_request_commits(base_ref, head_ref, offset, limit)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::CommitsBetween {
                            base_ref: base_ref.into(),
                            head_ref: head_ref.into(),
                            offset,
                            limit: limit.min(250),
                            raw_json: true,
                            pull_request: true,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Up to `limit` commits reachable from `head_ref` but not from `base_ref`, newest first. These
    /// are the commits of a pull request.
    pub async fn commits_between(
        &self,
        base_ref: &str,
        head_ref: &str,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        self.commits_between_page(base_ref, head_ref, 0, limit)
            .await
    }

    /// A bounded range page, newest first; skipped commit bodies are never loaded.
    pub async fn commits_between_page(
        &self,
        base_ref: &str,
        head_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        if limit > MAX_COMMIT_PAGE_SIZE {
            return Err(RepoError::response_too_large());
        }
        match &self.0 {
            Backend::Local(local) => {
                local
                    .commits_between(base_ref, head_ref, offset, limit)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::CommitsBetween {
                            base_ref: base_ref.into(),
                            head_ref: head_ref.into(),
                            limit,
                            offset,
                            raw_json: true,
                            pull_request: false,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Up to `limit` commits after `offset` that pass `filter` and changed `path`, newest first,
    /// from `git_ref`.
    pub async fn path_commits(
        &self,
        git_ref: &str,
        path: &str,
        offset: usize,
        limit: usize,
        filter: &CommitFilter,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        if offset > MAX_COMMIT_OFFSET || limit > MAX_COMMIT_PAGE_SIZE {
            return Err(RepoError::response_too_large());
        }
        match &self.0 {
            Backend::Local(local) => {
                local
                    .path_commits(git_ref, path, offset, limit, filter)
                    .await
            }
            Backend::Remote(remote) => {
                let operation = Operation::PathCommits {
                    git_ref: git_ref.into(),
                    path: path.into(),
                    offset,
                    limit,
                    filter: filter.clone(),
                    raw_json: true,
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

    /// Object IDs and the actual previous tag used to generate release notes, read on the primary.
    pub async fn release_history(
        &self,
        head: &str,
        previous: Option<&str>,
        exclude_tag: &str,
    ) -> Result<git::ReleaseHistory, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.release_history(head, previous, exclude_tag).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::ReleaseHistory {
                            head: head.into(),
                            previous: previous.map(str::to_owned),
                            exclude_tag: exclude_tag.into(),
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
                            stats: false,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// The counts and files of [`Self::diff`] with empty patches, so a file whose patch is too
    /// large to show does not stop them.
    pub async fn diff_stats(&self, base_ref: &str, head_ref: &str) -> Result<DiffInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.diff_stats(base_ref, head_ref).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::Diff {
                            base_ref: base_ref.into(),
                            head_ref: head_ref.into(),
                            stats: true,
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
        self.changed_paths_comparison(base_ref, head_ref, false, false)
            .await
    }

    /// Changed paths of a three-dot comparison as the diff view lists them: a rename is one path.
    pub async fn diff_paths(
        &self,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<Vec<String>, RepoError> {
        self.changed_paths_comparison(base_ref, head_ref, false, true)
            .await
    }

    /// Changed paths between push endpoints, or from the empty tree for a new ref.
    pub async fn push_changed_paths(
        &self,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<Vec<String>, RepoError> {
        self.changed_paths_comparison(base_ref, head_ref, true, false)
            .await
    }

    async fn changed_paths_comparison(
        &self,
        base_ref: &str,
        head_ref: &str,
        endpoints: bool,
        renames: bool,
    ) -> Result<Vec<String>, RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .changed_paths(base_ref, head_ref, endpoints, renames)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::ChangedPaths {
                            base_ref: base_ref.into(),
                            head_ref: head_ref.into(),
                            endpoints,
                            renames,
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
                        source: crate::wire::repository_uuid(&from.disk_path)?,
                        base_ref: base_ref.into(),
                        head_ref: head_ref.into(),
                    },
                    b"",
                )
                .await
            }
            _ => Err(crate::wire::invalid(
                "comparison repositories are not on the same node",
            )),
        }
    }

    /// Comparison counts, merge base and an oldest-first commit page, including fork heads.
    pub async fn comparison_from(
        &self,
        source: &RepoStore,
        base_ref: &str,
        head_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<CommitComparison, RepoError> {
        if limit > 250 {
            return Err(RepoError::response_too_large());
        }
        match (&self.0, &source.0) {
            (Backend::Local(base), Backend::Local(from)) => {
                base.comparison_from(from, base_ref, head_ref, offset, limit)
                    .await
            }
            (Backend::Remote(base), Backend::Remote(from)) if base.node_id == from.node_id => {
                base.call(
                    Operation::ComparisonFrom {
                        source: crate::wire::repository_uuid(&from.disk_path)?,
                        base_ref: base_ref.into(),
                        head_ref: head_ref.into(),
                        offset,
                        limit,
                        raw_json: true,
                    },
                    b"",
                )
                .await
            }
            _ => Err(crate::wire::invalid(
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

    /// Each head's divergence from `base`, in the order of `heads`, with the time of its commit.
    pub async fn branch_statuses(
        &self,
        base: &str,
        heads: &[String],
    ) -> Result<Vec<BranchStatus>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.branch_statuses(base, heads).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::BranchStatuses {
                            base: base.into(),
                            heads: heads.to_vec(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// The tree at `git_ref` as an archive in `format`, whose entries sit under `prefix/`.
    pub async fn archive(
        &self,
        git_ref: &str,
        prefix: &str,
        format: git::ArchiveFormat,
    ) -> Result<Vec<u8>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.archive(git_ref, prefix, format).await,
            Backend::Remote(remote) => {
                remote
                    .bytes_with_limit(
                        Operation::Archive {
                            git_ref: git_ref.into(),
                            prefix: prefix.into(),
                            format,
                        },
                        b"",
                        MAX_ARCHIVE_RESPONSE_BYTES,
                    )
                    .await
            }
        }
    }

    /// Primary-only default-branch statistics. Remote owners also publish the cache
    /// while holding their repository publication lease.
    pub(crate) async fn languages_from_primary(
        &self,
        branch: &str,
    ) -> Result<Vec<LanguageStat>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.languages(branch).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrimaryLanguages {
                            git_ref: branch.into(),
                        },
                        b"",
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

    /// Count-only history on the default branch, including merge commits.
    pub async fn branch_contributors(
        &self,
        branch: &str,
    ) -> Result<Vec<ContributorInfo>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.branch_contributors(branch).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::BranchContributors {
                            branch: branch.into(),
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

    /// Bounded, ref-cached commit and author statistics from this repository's storage node.
    pub async fn commit_statistics(
        &self,
        branch: &str,
        mode: git::StatisticsMode,
    ) -> Result<git::CommitStatistics, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.commit_statistics(branch, mode).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::CommitStatistics {
                            branch: branch.into(),
                            raw_json: true,
                            mode,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Lines added and removed on `branch` in each of the last 52 weeks, oldest first.
    pub async fn code_frequency(&self, branch: &str) -> Result<Vec<WeekChanges>, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.code_frequency(branch).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::CodeFrequency {
                            branch: branch.into(),
                        },
                        b"",
                    )
                    .await
            }
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
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        source_repo_id: i64,
        actor: impl Into<PullHeadActor>,
        source: &RepoStore,
        number: i32,
        head: &str,
        expected_head: Option<&str>,
    ) -> Result<(), RepoError> {
        let actor = actor.into();
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
                    actor.user,
                    Operation::FetchPullHead {
                        token: actor.token,
                        source: crate::wire::repository_uuid(&from.disk_path)?,
                        number,
                        head: head.into(),
                        expected_head: expected_head.map(str::to_owned),
                    },
                    b"",
                )
                .await
            }
            _ => Err(crate::wire::invalid(
                "fork source is not on the destination node",
            )),
        }
    }

    /// Renames a branch and follows it in protection and default-branch metadata.
    pub async fn rename_branch_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        old: &str,
        new: &str,
    ) -> Result<String, RepoError> {
        let authority = actor.into();
        match &self.0 {
            Backend::Local(local) => {
                local
                    .rename_branch(pool, repo_id, authority, old, new)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        authority.user_id,
                        Operation::RenameBranch {
                            old: old.into(),
                            new: crate::wire::PublicationText::retain(new, authority),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Syncs a fork branch from its direct parent without discarding local commits.
    pub async fn merge_upstream_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        source: (i64, &RepoStore),
        branch: &str,
        token: Option<i64>,
    ) -> Result<UpstreamMerge, RepoError> {
        match (&self.0, &source.1 .0) {
            (Backend::Local(local), Backend::Local(from)) => {
                local
                    .merge_upstream(pool, repo_id, actor, (source.0, from), branch, token)
                    .await
            }
            (Backend::Remote(remote), Backend::Remote(from)) if remote.node_id == from.node_id => {
                remote
                    .call_as(
                        actor,
                        Operation::MergeUpstream {
                            source: crate::wire::repository_uuid(&from.disk_path)?,
                            branch: branch.into(),
                            token,
                        },
                        b"",
                    )
                    .await
            }
            _ => Err(crate::wire::invalid(
                "fork source is not on the destination node",
            )),
        }
    }

    /// Creates a branch at the commit `sha`; returns that commit's ID.
    pub async fn create_branch_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        name: &str,
        sha: &str,
    ) -> Result<String, RepoError> {
        let actor = actor.into();
        if git::branch_ref(name).is_none() {
            return Err(invalid_ref("branch"));
        }
        match &self.0 {
            Backend::Local(local) => local.create_branch(pool, repo_id, actor, name, sha).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor.user_id,
                        Operation::CreateBranch {
                            name: name.into(),
                            sha: crate::wire::PublicationText::retain(sha, actor),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    pub async fn transfer_repository_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        new_owner: &str,
    ) -> Result<Repository, RepoError> {
        let actor = actor.into();
        match &self.0 {
            Backend::Local(local) => {
                local
                    .transfer_repository(pool, repo_id, actor, new_owner)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor.user_id,
                        Operation::TransferRepository {
                            new_owner: crate::wire::PublicationText::retain(new_owner, actor),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The store forwards the dependency-security request's explicit fields."
    )]
    pub async fn configure_dependency_security_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        credential: codeyard_db_core::queries::users::accounts::OwnedAccountCredential,
        feature: codeyard_db_repos::repositories::dependencies::Feature,
        enabled: bool,
        private_consent: bool,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .configure_dependency_security(
                        pool,
                        repo_id,
                        actor,
                        credential,
                        feature,
                        enabled,
                        private_consent,
                    )
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::ConfigureDependencySecurity {
                            credential,
                            feature,
                            enabled,
                            private_consent,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Publishes release metadata/proof while its tag is pinned by the primary Git lock.
    pub async fn mutate_release_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        mutation: crate::repositories::releases::immutable::Mutation,
        attestation: Option<codeyard_model::release_integrity::Attestation>,
    ) -> Result<(codeyard_db_core::models::Release, bool), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .mutate_release(pool, repo_id, actor, mutation, attestation)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::MutateRelease {
                            mutation,
                            attestation,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Changes the default and metadata under one Git lock and one guarded SQL transaction.
    pub async fn patch_repository_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        name: &str,
        changes: &codeyard_db_repos::repositories::records::RepositoryChanges<&str>,
    ) -> Result<Repository, RepoError> {
        if git::branch_ref(name).is_none() {
            return Err(invalid_ref("branch"));
        }
        let changes = changes.owned();
        let actor = actor.into();
        match &self.0 {
            Backend::Local(local) => {
                local
                    .patch_repository(pool, repo_id, actor, name, changes)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor.user_id,
                        Operation::PatchRepository {
                            default_branch: crate::wire::PublicationText::retain(name, actor),
                            changes,
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Makes `name` the default branch: HEAD follows it, so clones made afterwards get it too.
    pub async fn set_default_branch_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        name: &str,
    ) -> Result<(), RepoError> {
        let actor = actor.into();
        if git::branch_ref(name).is_none() {
            return Err(invalid_ref("branch"));
        }
        match &self.0 {
            Backend::Local(local) => local.set_default_branch(pool, repo_id, actor, name).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor.user_id,
                        Operation::SetDefaultBranch {
                            name: crate::wire::PublicationText::retain(name, actor),
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
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        name: &str,
    ) -> Result<(), RepoError> {
        let actor = actor.into();
        if git::branch_ref(name).is_none() {
            return Err(invalid_ref("branch"));
        }
        match &self.0 {
            Backend::Local(local) => local.delete_branch(pool, repo_id, actor, name).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor.user_id,
                        Operation::DeleteBranch {
                            name: crate::wire::PublicationText::retain(name, actor),
                        },
                        b"",
                    )
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
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        name: &str,
        sha: &str,
        message: Option<&str>,
        tagger_name: &str,
        tagger_email: &str,
    ) -> Result<TagInfo, RepoError> {
        let actor = actor.into();
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
                        actor.user_id,
                        Operation::CreateTag {
                            name: name.into(),
                            sha: crate::wire::PublicationText::retain(sha, actor),
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
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        name: &str,
    ) -> Result<(), RepoError> {
        let actor = actor.into();
        if git::tag_ref(name).is_none() {
            return Err(invalid_ref("tag"));
        }
        match &self.0 {
            Backend::Local(local) => local.delete_tag(pool, repo_id, actor, name).await,
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor.user_id,
                        Operation::DeleteTag {
                            name: crate::wire::PublicationText::retain(name, actor),
                        },
                        b"",
                    )
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
        self.prepare_file_with_identities(
            write,
            content,
            author,
            email,
            &CommitIdentities::default(),
        )
        .await
    }

    pub async fn prepare_file_with_identities(
        &self,
        write: &FileWrite,
        content: &[u8],
        author: &str,
        email: &str,
        identities: &CommitIdentities,
    ) -> Result<PreparedFile, RepoError> {
        write.validate()?;
        identities.validate()?;
        let prepared = match &self.0 {
            Backend::Local(local) => {
                local
                    .prepare_file_with_identities(write, content, author, email, identities)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrepareFile {
                            write: write.clone(),
                            author: author.into(),
                            email: email.into(),
                            identities: identities.clone(),
                        },
                        content,
                    )
                    .await
            }
        }?;
        identities.check_prepared(&prepared.commit)?;
        Ok(prepared)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "File save has explicit authority and commit identity inputs."
    )]
    pub async fn save_file_as(
        &self,
        pool: &codeyard_db_core::DbPool,
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
                local
                    .publish_file(pool, repo_id, actor, &prepared, None)
                    .await
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
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        prepared: &PreparedFile,
    ) -> Result<CommitInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .publish_file(pool, repo_id, actor, prepared, None)
                    .await
            }
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

    pub async fn publish_dependency_update_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        prepared: &PreparedFile,
        publication: &codeyard_db_repos::repositories::dependencies::Publication,
    ) -> Result<CommitInfo, RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                let (pool, local, prepared, publication) = (
                    pool.clone(),
                    local.clone(),
                    prepared.clone(),
                    publication.clone(),
                );
                tokio::spawn(async move {
                    local
                        .publish_file(&pool, repo_id, actor, &prepared, Some(&publication))
                        .await
                })
                .await
                .map_err(|_| {
                    RepoError::new(
                        ErrorKind::Unavailable,
                        "Dependency update publication interrupted",
                    )
                })?
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishDependencyUpdate {
                            prepared: prepared.clone(),
                            publication: Box::new(publication.clone()),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Updates a pull head from a readable base, enforcing direct-push protection and head CAS.
    pub async fn update_pull_branch_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        source: (i64, &RepoStore),
        request: &PullBranchUpdate,
    ) -> Result<String, RepoError> {
        match (&self.0, &source.1 .0) {
            (Backend::Local(target), Backend::Local(from)) => {
                target
                    .update_pull_branch(pool, repo_id, actor, (source.0, from), request)
                    .await
            }
            (Backend::Remote(target), Backend::Remote(from)) if target.node_id == from.node_id => {
                target
                    .call_as(
                        actor,
                        Operation::UpdatePullBranch {
                            source: crate::wire::repository_uuid(&from.disk_path)?,
                            request: request.clone(),
                        },
                        b"",
                    )
                    .await
            }
            _ => Err(crate::wire::invalid(
                "pull request source is not on the destination node",
            )),
        }
    }

    /// Builds an inverse change without touching refs. Publication uses normal branch authority.
    pub async fn prepare_revert(&self, request: &RevertRequest) -> Result<String, RepoError> {
        match &self.0 {
            Backend::Local(local) => local.prepare_revert(request).await,
            Backend::Remote(remote) => {
                remote
                    .call(
                        Operation::PrepareRevert {
                            request: request.clone(),
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

    /// Rechecks the durable request and original token under the owning repository lock.
    pub async fn publish_async_merge_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        request: &AsyncMergePublication,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                local
                    .publish_async_merge(pool, repo_id, actor, request)
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishAsyncMerge {
                            request: request.clone(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Publishes a direct branch merge with live write policy and branch-tip CAS. A branch merge
    /// cannot stand in for an approved pull request, even when its object has two parents.
    pub async fn publish_branch_merge_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        publication: &BranchMergePublication,
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => {
                let heads: Vec<_> = publication
                    .head_branch
                    .as_deref()
                    .map(|branch| (branch, publication.head.as_str()))
                    .into_iter()
                    .collect();
                local
                    .publish_merge(
                        pool,
                        repo_id,
                        actor,
                        &heads,
                        &publication.base_branch,
                        &publication.base,
                        &publication.result,
                        MergeMethod::Merge,
                        WriteVia::Direct,
                        publication.authority.token_id(),
                        Some(&publication.authority),
                        None,
                    )
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishBranchMerge {
                            publication: publication.clone(),
                        },
                        b"",
                    )
                    .await
            }
        }
    }

    /// Publishes a prepared pull-request merge by moving `base_branch` to `result`,
    /// failing if either branch is no longer at `base` or `head`.
    #[expect(
        clippy::too_many_arguments,
        reason = "Merge publication carries explicit authority and CAS inputs."
    )]
    pub async fn publish_merge_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        head_branch: &str,
        base_branch: &str,
        head: &str,
        base: &str,
        result: &str,
        method: MergeMethod,
        policy: Option<codeyard_db_repos::pull_requests::merge_policy::Snapshot>,
    ) -> Result<(), RepoError> {
        let authority = actor.into();
        let actor = authority.user_id;
        let token = authority.token_id();
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
                        WriteVia::Merge,
                        token,
                        Some(&authority),
                        policy,
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
                            token: crate::wire::PublicationCredential::retain(authority),
                            policy,
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
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        heads: &[(String, String)],
        base_branch: &str,
        base: &str,
        result: &str,
        policy: Option<codeyard_db_repos::pull_requests::merge_policy::Snapshot>,
    ) -> Result<(), RepoError> {
        let authority = actor.into();
        let actor = authority.user_id;
        let token = authority.token_id();
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
                        WriteVia::Merge,
                        token,
                        Some(&authority),
                        policy,
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
                            token: crate::wire::PublicationCredential::retain(authority),
                            policy,
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

    /// Moves each branch in `moves` to its new commit, provided each is still at its old one,
    /// `base_branch` is still at `base`, and stack `stack_id` still has the `versions` the rebase
    /// read. Nothing moves unless every check passes.
    #[expect(
        clippy::too_many_arguments,
        reason = "Restack publication carries explicit authority, stack and CAS inputs."
    )]
    pub async fn publish_restack_as(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
        stack_id: i64,
        versions: &[(i64, i64)],
        base_branch: &str,
        base: &str,
        moves: &[RestackMove],
    ) -> Result<(), RepoError> {
        let authority = actor.into();
        let actor = authority.user_id;
        match &self.0 {
            Backend::Local(local) => {
                local
                    .publish_restack(
                        pool,
                        repo_id,
                        authority,
                        stack_id,
                        versions,
                        base_branch,
                        base,
                        moves,
                    )
                    .await
            }
            Backend::Remote(remote) => {
                remote
                    .call_as(
                        actor,
                        Operation::PublishRestack {
                            stack_id,
                            versions: versions.to_vec(),
                            base_branch: crate::wire::PublicationText::retain(
                                base_branch,
                                authority,
                            ),
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

    /// Whether this store runs Git on this node. Its sessions take a local slot, and a remote
    /// store's take a forwarding slot.
    pub fn is_local(&self) -> bool {
        matches!(self.0, Backend::Local(_))
    }

    /// A slot for one Git session: a local one runs Git here, a remote one forwards it.
    pub async fn session_permit(&self) -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
        match &self.0 {
            Backend::Local(_) => process::session_permit().await,
            Backend::Remote(_) => process::forwarding_permit().await,
        }
    }

    /// Pipes the client to Git on the owning node, with the shared receive hooks.
    pub async fn git_session(
        &self,
        session: GitSession,
        receive: Option<(
            codeyard_db_core::DbPool,
            std::sync::Arc<dyn crate::receive_ci::ReceiveCi>,
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
            Backend::Remote(_) => Err(crate::wire::invalid("HEAD is read on the owning node")),
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
                Err(crate::wire::invalid("replica packs come from the primary").into())
            }
        }
    }

    /// Stores a pack from the primary in this copy.
    pub async fn index_pack(&self, pack: &Path, strict: bool) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.index_pack(pack, strict).await,
            Backend::Remote(_) => Err(crate::wire::invalid("replica copies live on this node")),
        }
    }

    /// Offline restore installs the database's canonical refs on an unbound local mirror.
    pub async fn restore_refs(
        &self,
        refs: &[codeyard_db_core::models::RefValue],
    ) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.restore_refs(refs).await,
            Backend::Remote(_) => Err(crate::wire::invalid(
                "restore requires an offline local mirror",
            )),
        }
    }

    /// Makes this copy's refs exactly `refs`.
    pub async fn set_refs(&self, refs: &HashMap<String, String>) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.set_refs(refs).await,
            Backend::Remote(_) => Err(crate::wire::invalid("replica copies live on this node")),
        }
    }

    /// Points this copy's HEAD at the primary's branch.
    pub async fn set_head_target(&self, target: &str) -> Result<(), RepoError> {
        match &self.0 {
            Backend::Local(local) => local.set_head_target(target).await,
            Backend::Remote(_) => Err(crate::wire::invalid("replica copies live on this node")),
        }
    }

    /// This node's directory for the repository, or `None` when another node holds it.
    pub fn local_dir(&self) -> Option<&Path> {
        match &self.0 {
            Backend::Local(local) => Some(local.path()),
            Backend::Remote(_) => None,
        }
    }

    /// The local directory for the host OpenSSH streaming gateway only.
    /// HTTP and built-in SSH use streamed store sessions instead.
    pub fn local_path_for_spawning_git(&self) -> Result<&Path, RepoError> {
        match &self.0 {
            Backend::Local(local) => Ok(local.path()),
            Backend::Remote(_) => Err(crate::wire::invalid(
                "Host OpenSSH cannot serve remote repositories; use HTTP or built-in SSH",
            )),
        }
    }
}
