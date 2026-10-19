//! The local store: the bare repository is a directory on this machine. Each
//! method runs the matching Git code, moving blocking libgit2 work off the async
//! threads and turning failures into [`RepoError`]s.
use super::{
    allowed_merge_methods, editor, BlobBytes, BlobRange, ErrorKind, FileWrite, GitService,
    MergeMethod, MergeRequest, RepoError, RestackMove, MAX_ARCHIVE_RESPONSE_BYTES,
    MAX_BLOB_RESPONSE_BYTES,
};
use crate::backend::{
    db,
    git::{
        self, repository::process, BlobContent, BranchStatus, CiCommit, CiWorkflowFile,
        CiWorkflowSource, CommitActivityLog, CommitFilter, CommitInfo, ContributorInfo, DiffInfo,
        FileBlame, FileList, LanguageStat, RepoInfo, TagInfo, TreeEntry,
    },
    repositories::{
        access, languages,
        protection::{
            check_branch_write, BranchWrite, History, Refusal, WriteVia, MERGE_COMMIT_BLOCKED,
        },
    },
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Tries to remove a repository directory this many times. A Git process that is
/// still writing into it can make the first attempt fail.
const REMOVE_ATTEMPTS: u32 = 3;
const REMOVE_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Output limit for the short Git commands that run against a repository directory.
const MAX_GIT_OUTPUT: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct LocalStore {
    disk_path: String,
    /// The storage node this store was opened for. A write is refused once the repository
    /// row names another node, so a store opened before a promotion cannot publish on the
    /// old primary afterwards. `None` for a standalone repository or a bare path.
    node_id: Option<i64>,
}

fn unavailable(message: &'static str) -> RepoError {
    RepoError::new(ErrorKind::Unavailable, message)
}

/// A full object ID: 40 hex digits, as the primary sends them.
fn is_object_id(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize_blob_range(range: BlobRange, size: u64) -> Result<(u64, usize), RepoError> {
    let (offset, length) = match range {
        BlobRange::Full => (0, size),
        BlobRange::Window { offset, length } => {
            let offset = offset.min(size);
            (offset, length.min(size - offset))
        }
        BlobRange::Bytes { start, end } => {
            if start >= size || end.is_some_and(|end| end < start) {
                return Err(RepoError::range_not_satisfiable());
            }
            let end = end.unwrap_or(size - 1).min(size - 1);
            (start, end - start + 1)
        }
        BlobRange::Suffix { length } => {
            if length == 0 || size == 0 {
                return Err(RepoError::range_not_satisfiable());
            }
            let length = length.min(size);
            (size - length, length)
        }
    };
    let length = usize::try_from(length).map_err(|_| RepoError::response_too_large())?;
    if length > MAX_BLOB_RESPONSE_BYTES {
        return Err(RepoError::response_too_large());
    }
    Ok((offset, length))
}

fn repository_matches_path(
    repository: &db::models::Repository,
    path: &Path,
    node_id: Option<i64>,
) -> bool {
    if node_id.is_some() && repository.storage_node_id != node_id {
        return false;
    }
    if repository.storage_node_id.is_none() {
        Path::new(&repository.disk_path) == path
    } else {
        crate::backend::cluster::rpc::repository_uuid(&repository.disk_path)
            .ok()
            .zip(path.file_name().and_then(|name| name.to_str()))
            .is_some_and(|(uuid, filename)| filename == format!("{uuid}.git"))
    }
}

async fn live_read(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    path: &Path,
    node_id: Option<i64>,
) -> Result<(), RepoError> {
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| RepoError::authority_unavailable())?
        .filter(|user| !user.is_banned && !user.must_change_password)
        .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "repository read denied"))?;
    let repository =
        access::current_repository(pool, repo_id)
            .await
            .map_err(|error| match error {
                access::AccessError::Unavailable => RepoError::authority_unavailable(),
                _ => RepoError::new(ErrorKind::Invalid, "repository read denied"),
            })?;
    if !access::can_read(pool, Some(&user), &repository)
        .await
        .map_err(|_| RepoError::authority_unavailable())?
        || !repository_matches_path(&repository, path, node_id)
    {
        return Err(RepoError::new(ErrorKind::Invalid, "repository read denied"));
    }
    Ok(())
}

/// Checks the actor's write access and, for a branch write, the branch protection rules, at
/// the moment the ref is about to move. `history` says what the new commits of a branch write
/// look like. Tag writes pass `None` and `&History::SingleCommit`.
async fn live_write(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    branch: Option<&BranchWrite<'_>>,
    history: &History<'_>,
    path: &Path,
    node_id: Option<i64>,
) -> Result<db::models::Repository, RepoError> {
    let (repository, user) = access::check_write(pool, repo_id, actor)
        .await
        .map_err(|error| match error {
            access::AccessError::Unavailable => RepoError::authority_unavailable(),
            _ => RepoError::write_denied(),
        })?;
    if !repository_matches_path(&repository, path, node_id) {
        return Err(RepoError::write_denied());
    }
    if let Some(write) = branch {
        match check_branch_write(pool, &repository, &user, write, history).await {
            Ok(_) => {}
            Err(Refusal::Blocked(MERGE_COMMIT_BLOCKED)) => {
                return Err(RepoError::merge_commit_blocked())
            }
            Err(Refusal::Blocked(reason)) => return Err(RepoError::protected_refusal(reason)),
            Err(Refusal::Unavailable) => return Err(RepoError::authority_unavailable()),
        }
    }
    Ok(repository)
}

/// Records a branch update made by the web, the API or a merge as profile activity, and
/// refreshes the stored languages when the branch is the default branch. Git has already moved
/// the branch, so a failure is logged rather than reported as a failed write.
async fn record_branch_update(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    path: &Path,
    branch: &str,
    old: Option<&str>,
    new: &str,
) {
    languages::refresh_default_branch(pool.clone(), repo_id, path.to_owned(), branch.to_owned());
    let reference = format!("refs/heads/{branch}");
    let count = match git::count_pushed_commits(path, old, new, &reference).await {
        Ok(count) => i32::try_from(count).unwrap_or(i32::MAX),
        Err(error) => {
            tracing::warn!(repo_id, %error, "could not count a branch update");
            return;
        }
    };
    // A save must not wait behind a busy database for the sake of a profile feed entry.
    let record = db::activity::record_repository_push(pool, repo_id, actor, branch, count);
    match tokio::time::timeout(std::time::Duration::from_millis(500), record).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(repo_id, %error, "could not record a branch update"),
        Err(_) => tracing::warn!(
            repo_id,
            "gave up recording a branch update on a busy database"
        ),
    }
}

/// Counts a ref change that Git has already made, so replicas know to copy it. The change
/// is in place, so the request still succeeds if counting keeps failing: reporting it would
/// leave callers, such as a merged pull request, disagreeing with Git. A short outage of the
/// database is retried; replicas otherwise catch up at the next write.
pub(crate) async fn record_ref_change(pool: &db::DbPool, repo_id: i64) {
    let mut delay = std::time::Duration::from_millis(100);
    for attempt in 1..=3 {
        match db::cluster::bump_write_version(pool, repo_id).await {
            Ok(_) => return,
            Err(error) if attempt == 3 => {
                tracing::error!(repo_id, %error, "could not record a ref change for replicas");
            }
            Err(error) => {
                tracing::warn!(repo_id, %error, "could not record a ref change; retrying");
                tokio::time::sleep(delay).await;
                delay *= 4;
            }
        }
    }
}

/// Refuses `created` when a repository that holds `held` would pass the count limit or the
/// byte limit on the names of `what`. `created` names the full refs.
fn ensure_room(
    held: git::RefsInUse,
    created: &[&str],
    what: &str,
    limit: usize,
    byte_limit: usize,
) -> Result<(), RepoError> {
    if held.count + created.len() > limit {
        return Err(RepoError::new(
            ErrorKind::Invalid,
            format!("A repository holds at most {limit} {what}"),
        ));
    }
    let added: usize = created
        .iter()
        .map(|name| git::ref_listing_bytes(name))
        .sum();
    if held.bytes + added > byte_limit {
        return Err(RepoError::new(
            ErrorKind::Invalid,
            format!(
                "The names of a repository's {what} may total at most {} KiB",
                byte_limit / 1024
            ),
        ));
    }
    Ok(())
}

/// Refuses `created` when a repository that holds `held` would pass the limit on branches
/// and tags, or on the bytes of their names. `created` names the full refs.
pub(crate) fn ensure_ref_room(held: git::RefsInUse, created: &[&str]) -> Result<(), RepoError> {
    ensure_room(
        held,
        created,
        "branches and tags",
        git::MAX_REPOSITORY_REFS,
        git::MAX_REPOSITORY_REF_BYTES,
    )
}

/// Refuses a new pull request head once the repository holds as many as the mirror limit
/// leaves after branches and tags, so a replica can still list every ref.
fn ensure_pull_head_room(held: git::RefsInUse, reference: &str) -> Result<(), RepoError> {
    ensure_room(
        held,
        &[reference],
        "pull request heads",
        git::MAX_MIRROR_REFS - git::MAX_REPOSITORY_REFS,
        git::MAX_MIRROR_REF_BYTES - git::MAX_REPOSITORY_REF_BYTES,
    )
}

impl LocalStore {
    pub(super) fn new(disk_path: &str, node_id: Option<i64>) -> Self {
        Self {
            disk_path: disk_path.to_owned(),
            node_id,
        }
    }

    pub(super) fn disk_path(&self) -> &str {
        &self.disk_path
    }

    pub(super) fn path(&self) -> &Path {
        Path::new(&self.disk_path)
    }

    /// Runs blocking Git code against this repository on a worker thread.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, RepoError> {
        let path: PathBuf = self.path().to_owned();
        let permit =
            git::repository::process::operation_slot(git::repository::process::RPC_TIMEOUT)
                .await
                .map_err(|_| unavailable("Git operation timed out"))?;
        match tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(&path)
        })
        .await
        {
            Ok(result) => result.map_err(RepoError::from),
            Err(_) => Err(unavailable("Git did not finish")),
        }
    }

    /// Refuses a new branch or tag once the repository is at the limit.
    async fn ensure_room_for_ref(&self, reference: &str) -> Result<(), RepoError> {
        let held = self.blocking(git::branches_and_tags_in_use).await?;
        ensure_ref_room(held, &[reference])
    }

    // ----- Opening and lifecycle -----

    pub(super) async fn create(
        repos_root: &Path,
        default_branch: &str,
        source: Option<&LocalStore>,
    ) -> Result<Self, RepoError> {
        let refused = || unavailable("Repository storage is unavailable.");
        let root = std::path::absolute(repos_root).map_err(|_| refused())?;
        let disk_path = root
            .join(format!("{}.git", Uuid::new_v4()))
            .to_str()
            .ok_or_else(refused)?
            .to_owned();
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|_| refused())?;
        let store = Self {
            disk_path,
            node_id: None,
        };
        if let Err(error) = store.initialize(source, default_branch).await {
            // A leftover directory is logged by `remove`; the creation error is the
            // one to report.
            let _ = store.remove().await;
            return Err(error);
        }
        Ok(store)
    }

    pub(super) async fn initialize(
        &self,
        source: Option<&LocalStore>,
        default_branch: &str,
    ) -> Result<(), RepoError> {
        git::initialize_repository(self.path(), source.map(LocalStore::path), default_branch)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn initialize_fork(
        &self,
        pool: &db::DbPool,
        source_repo_id: i64,
        actor: i64,
        source: &LocalStore,
        default_branch: &str,
    ) -> Result<(), RepoError> {
        // The row is already discoverable. Keep copied objects outside its live path
        // until the source-access check, on the same filesystem for atomic publication.
        let temporary = tempfile::Builder::new()
            .prefix(".fork-")
            .tempdir_in(
                self.path()
                    .parent()
                    .ok_or_else(|| unavailable("Invalid storage path"))?,
            )
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        git::initialize_repository(temporary.path(), Some(source.path()), default_branch)
            .await
            .map_err(RepoError::from)?;
        live_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
        tokio::fs::rename(temporary.path(), self.path())
            .await
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))
    }

    pub(super) async fn remove(&self) -> Result<(), RepoError> {
        for attempt in 1..=REMOVE_ATTEMPTS {
            match tokio::fs::remove_dir_all(self.path()).await {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) if attempt < REMOVE_ATTEMPTS => {
                    tracing::debug!(path = %self.path().display(), %error, attempt, "retrying repository storage removal");
                    tokio::time::sleep(REMOVE_RETRY_DELAY * attempt).await;
                }
                Err(error) => {
                    tracing::warn!(path = %self.path().display(), %error, "failed to remove repository storage");
                    return Err(unavailable("Repository storage could not be removed."));
                }
            }
        }
        unreachable!("the last attempt returns")
    }

    pub(super) async fn exists(&self) -> bool {
        self.blocking(|path| Ok(git::is_git_repo(path)))
            .await
            .unwrap_or(false)
    }

    pub(super) async fn size_bytes(&self) -> Result<u64, RepoError> {
        self.blocking(crate::backend::git::stats::directory_size)
            .await
    }

    // ----- Reads -----

    pub(super) async fn refs(&self) -> Result<HashMap<String, String>, RepoError> {
        self.blocking(git::list_refs).await
    }

    /// Every ref, for a replica to copy.
    pub(super) async fn mirror_refs(&self) -> Result<HashMap<String, String>, RepoError> {
        self.blocking(git::list_mirror_refs).await
    }

    pub(super) async fn info(&self) -> Result<RepoInfo, RepoError> {
        git::get_repo_info(self.path())
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn commit_count(&self) -> Result<u64, RepoError> {
        git::count_commits(self.path())
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn tree(
        &self,
        git_ref: &str,
        path: &str,
    ) -> Result<Vec<TreeEntry>, RepoError> {
        git::list_tree(self.path(), git_ref, path)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn file_paths(
        &self,
        git_ref: &str,
        limit: usize,
    ) -> Result<FileList, RepoError> {
        git::list_file_paths(self.path(), git_ref, limit)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn workflow_files(
        &self,
        revision: &str,
    ) -> Result<Vec<CiWorkflowSource>, RepoError> {
        let revision = revision.to_owned();
        self.blocking(move |repo| git::list_ci_workflow_files(repo, &revision))
            .await
    }

    async fn read_blob_object_range(
        &self,
        id: &str,
        size: u64,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        let (offset, length) = normalize_blob_range(range, size)?;
        // The reader starts its own `git cat-file`, so it takes a slot like any other Git work.
        let _permit =
            git::repository::process::operation_slot(git::repository::process::RPC_TIMEOUT)
                .await
                .map_err(|_| unavailable("Git operation timed out"))?;
        let (bytes, is_binary) = git::read_blob_range(self.path(), id, size, offset, length)
            .await
            .map_err(RepoError::from)?;
        Ok(BlobBytes {
            id: id.to_owned(),
            bytes,
            is_binary,
            size,
            offset,
        })
    }

    pub(super) async fn blob(&self, git_ref: &str, path: &str) -> Result<BlobContent, RepoError> {
        let (owned_ref, owned_path) = (git_ref.to_owned(), path.to_owned());
        let (id, size) = self
            .blocking(move |repo| git::blob_metadata(repo, &owned_ref, &owned_path))
            .await?;
        let range = if size <= MAX_BLOB_RESPONSE_BYTES as u64 {
            BlobRange::Full
        } else {
            BlobRange::Window {
                offset: 0,
                length: 0,
            }
        };
        let blob = self.read_blob_object_range(&id, size, range).await?;
        let content = if blob.is_binary {
            format!("[Binary file, {} bytes]", size)
        } else {
            if size > MAX_BLOB_RESPONSE_BYTES as u64 {
                return Err(RepoError::response_too_large());
            }
            String::from_utf8_lossy(&blob.bytes).into_owned()
        };
        Ok(BlobContent {
            path: path.to_owned(),
            content,
            size,
            is_binary: blob.is_binary,
        })
    }

    pub(super) async fn blob_range(
        &self,
        git_ref: &str,
        path: &str,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        let (git_ref, path) = (git_ref.to_owned(), path.to_owned());
        let (id, size) = self
            .blocking(move |repo| git::blob_metadata(repo, &git_ref, &path))
            .await?;
        self.read_blob_object_range(&id, size, range).await
    }

    pub(super) async fn blob_by_id(&self, id: &str) -> Result<Vec<u8>, RepoError> {
        let id = id.to_owned();
        self.blocking(move |repo| git::get_blob_by_id(repo, &id))
            .await
    }

    /// Like `RepoStore::commits`, but the walk gives up at `deadline`, and its Git permit with it.
    pub(super) async fn commits_until(
        &self,
        git_ref: &str,
        offset: usize,
        limit: usize,
        filter: &CommitFilter,
        deadline: Instant,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        let (git_ref, filter) = (git_ref.to_owned(), filter.clone());
        self.blocking(move |repo| {
            git::list_commits(repo, &git_ref, offset, limit, &filter, deadline)
        })
        .await
    }

    pub(super) async fn search_code(
        &self,
        git_ref: &str,
        text: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<git::CodeMatch>, RepoError> {
        git::search_code(self.path(), git_ref, text, offset, limit)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn commits_between(
        &self,
        base_ref: &str,
        head_ref: &str,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        let base_ref = base_ref.to_owned();
        let head_ref = head_ref.to_owned();
        self.blocking(move |repo| git::list_range_commits(repo, &base_ref, &head_ref, limit))
            .await
    }

    pub(super) async fn path_commits(
        &self,
        git_ref: &str,
        path: &str,
        offset: usize,
        limit: usize,
        filter: &CommitFilter,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        git::list_path_commits(self.path(), git_ref, path, offset, limit, filter)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn branch_statuses(
        &self,
        base: &str,
        heads: &[String],
    ) -> Result<Vec<BranchStatus>, RepoError> {
        let (base, heads) = (base.to_owned(), heads.to_vec());
        self.blocking(move |repo| git::branch_statuses(repo, &base, &heads))
            .await
    }

    pub(super) async fn commit(&self, revision: &str) -> Result<CommitInfo, RepoError> {
        let revision = revision.to_owned();
        self.blocking(move |repo| git::get_commit(repo, &revision))
            .await
    }

    pub(super) async fn commit_diff(&self, revision: &str) -> Result<DiffInfo, RepoError> {
        git::get_commit_diff(self.path(), revision)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn diff(&self, base_ref: &str, head_ref: &str) -> Result<DiffInfo, RepoError> {
        git::get_diff(self.path(), base_ref, head_ref)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn diff_file(
        &self,
        base_ref: &str,
        head_ref: &str,
        path: &str,
    ) -> Result<DiffInfo, RepoError> {
        git::get_file_diff(self.path(), base_ref, head_ref, path)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn changed_paths(
        &self,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<Vec<String>, RepoError> {
        git::changed_paths(self.path(), base_ref, head_ref, false)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn diff_from(
        &self,
        source: &LocalStore,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<DiffInfo, RepoError> {
        git::compare_repositories(self.path(), source.path(), base_ref, head_ref)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn tags(&self) -> Result<Vec<TagInfo>, RepoError> {
        self.blocking(git::list_tags).await
    }

    pub(super) async fn archive(
        &self,
        git_ref: &str,
        prefix: &str,
        format: git::ArchiveFormat,
    ) -> Result<Vec<u8>, RepoError> {
        let git_ref = git_ref.to_owned();
        let revision = self
            .blocking(move |repo| git::resolve_archive_revision(repo, &git_ref))
            .await?;
        git::archive(
            self.path(),
            revision,
            prefix,
            format,
            MAX_ARCHIVE_RESPONSE_BYTES,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn languages(&self, git_ref: &str) -> Result<Vec<LanguageStat>, RepoError> {
        git::compute_language_stats(self.path(), git_ref)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn blame(&self, git_ref: &str, path: &str) -> Result<FileBlame, RepoError> {
        let (git_ref, path) = (git_ref.to_owned(), path.to_owned());
        self.blocking(move |repo| git::blame_file(repo, &git_ref, &path))
            .await
    }

    pub(super) async fn commit_activity(
        &self,
        git_ref: &str,
    ) -> Result<CommitActivityLog, RepoError> {
        git::read_commit_activity(self.path(), git_ref)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn contributors(&self) -> Result<Vec<ContributorInfo>, RepoError> {
        git::get_contributors(self.path())
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn is_ancestor(
        &self,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, RepoError> {
        git::commit_is_ancestor(self.path(), ancestor, descendant)
            .await
            .map_err(RepoError::from)
    }

    // ----- CI -----

    pub(super) async fn resolve_ci_commit(
        &self,
        reference: &str,
        sha: Option<&str>,
        must_be_on_ref: bool,
    ) -> Result<CiCommit, RepoError> {
        let (reference, sha) = (reference.to_owned(), sha.map(str::to_owned));
        self.blocking(move |repo| {
            git::resolve_ci_commit(repo, &reference, sha.as_deref(), must_be_on_ref)
        })
        .await
    }

    pub(super) async fn ci_workflow_file(
        &self,
        sha: &str,
        workflow_path: &str,
    ) -> Result<CiWorkflowFile, RepoError> {
        let (sha, workflow_path) = (sha.to_owned(), workflow_path.to_owned());
        self.blocking(move |repo| git::read_ci_workflow(repo, &sha, &workflow_path))
            .await
    }

    pub(super) async fn checkout(&self, destination: &Path, sha: &str) -> Result<(), RepoError> {
        git::checkout(self.path(), destination, sha)
            .await
            .map_err(RepoError::from)
    }

    /// Materialize a remote upload-pack response on the calling CI node.
    pub(super) async fn checkout_pack(
        destination: &Path,
        sha: &str,
        mut pack: tempfile::NamedTempFile,
    ) -> Result<(), RepoError> {
        let destination = destination.to_owned();
        let sha = sha.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Seek};
            pack.as_file_mut()
                .rewind()
                .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
            let mut header = [0; 8];
            pack.read_exact(&mut header)
                .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
            if &header != b"0008NAK\n" {
                return Err(unavailable("Invalid upload-pack response"));
            }
            std::fs::create_dir(&destination)
                .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
            let result = (|| -> anyhow::Result<()> {
                // A cancelled run removes its workspace while this task may still be
                // running. Never create the directory again behind it.
                let mut options = git2::RepositoryInitOptions::new();
                options.mkdir(false).mkpath(false);
                let repo = git2::Repository::init_opts(&destination, &options)?;
                let odb = repo.odb()?;
                let mut writer = odb.packwriter()?;
                std::io::copy(&mut pack, &mut writer)?;
                writer.commit()?;
                let oid = git2::Oid::from_str(&sha)?;
                let commit = git::repository::find_commit_checked(&repo, oid)?;
                repo.set_head_detached(oid)?;
                repo.checkout_tree(commit.as_object(), None)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_dir_all(&destination);
            }
            result.map_err(RepoError::from)
        })
        .await
        .map_err(|_| unavailable("Git did not finish"))?
    }

    // ----- Writes -----

    #[expect(
        clippy::too_many_arguments,
        reason = "Pull-head publication carries explicit source, actor, revision and CAS inputs."
    )]
    pub(super) async fn fetch_pull_head(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        source_repo_id: i64,
        actor: i64,
        source: &LocalStore,
        number: i32,
        head: &str,
        expected_head: Option<&str>,
    ) -> Result<(), RepoError> {
        let current = git::repository::current_pull_head(self.path(), number)
            .await
            .map_err(RepoError::from)?;
        // An authorized snapshot that is already published remains usable after
        // source access is revoked. No source object is read in this case.
        if current.as_deref() == Some(head) {
            return Ok(());
        }
        if current.as_deref() != expected_head {
            return Err(RepoError::from(anyhow::Error::new(
                git::repository::PullHeadConflict,
            )));
        }

        live_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
        let prepared = git::repository::prepare_pull_head(source.path(), head)
            .await
            .map_err(RepoError::from)?;
        self.publish_fetched_pull_head(
            pool,
            repo_id,
            source_repo_id,
            actor,
            source,
            number,
            head,
            expected_head,
            prepared,
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The effect-point check retains every pull-head publication input."
    )]
    pub(super) async fn publish_fetched_pull_head(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        source_repo_id: i64,
        actor: i64,
        source: &LocalStore,
        number: i32,
        head: &str,
        expected_head: Option<&str>,
        prepared: git::repository::PreparedPullHead,
    ) -> Result<(), RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        let current = git::repository::current_pull_head(self.path(), number)
            .await
            .map_err(RepoError::from)?;
        if current.as_deref() == Some(head) {
            return Ok(());
        }
        if current.as_deref() != expected_head {
            return Err(RepoError::from(anyhow::Error::new(
                git::repository::PullHeadConflict,
            )));
        }
        // Only a new head takes a slot. Moving an existing one keeps the count.
        if current.is_none() {
            let reference = format!("refs/pull/{number}/head");
            let held = self.blocking(git::pull_heads_in_use).await?;
            ensure_pull_head_room(held, &reference)?;
        }
        // The fetch happened only in temporary storage. Recheck live authority
        // after waiting for the destination lock and immediately before
        // receive-pack can migrate objects and publish the exact CAS ref.
        live_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
        git::repository::publish_pull_head(self.path(), number, head, expected_head, &prepared)
            .await
            .map_err(RepoError::from)?;
        record_ref_change(pool, repo_id).await;
        Ok(())
    }

    pub(super) async fn create_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        name: &str,
        sha: &str,
    ) -> Result<String, RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        let reference = git::branch_ref(name).ok_or_else(|| super::invalid_ref("branch"))?;
        self.ensure_room_for_ref(&reference).await?;
        let (owned_name, owned_sha) = (name.to_owned(), sha.to_owned());
        let target = self
            .blocking(move |repo| git::prepare_create_branch(repo, &owned_name, &owned_sha))
            .await?;
        let write = BranchWrite {
            branch: name,
            old: None,
            new: Some(target.as_str()),
            via: WriteVia::Direct,
        };
        // The branch points at a commit that is already stored, not at one the editor has just
        // written, so the merge commits in its history are read from the repository.
        live_write(
            pool,
            repo_id,
            actor,
            Some(&write),
            &History::Stored {
                repository: self.path(),
            },
            self.path(),
            self.node_id,
        )
        .await?;
        git::publish_ref_create(self.path(), &reference, &target)
            .await
            .map_err(RepoError::from)?;
        let defaults = self.synchronize_default_branch(pool, repo_id).await;
        record_ref_change(pool, repo_id).await;
        defaults?;
        Ok(target)
    }

    pub(super) async fn delete_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        name: &str,
    ) -> Result<(), RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        if git::repository::default_branch(self.path())
            .await
            .map_err(RepoError::from)?
            .as_deref()
            == Some(name)
        {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Cannot delete the default branch",
            ));
        }
        let owned_name = name.to_owned();
        let old = self
            .blocking(move |repo| git::prepare_delete_branch(repo, &owned_name))
            .await?;
        let write = BranchWrite {
            branch: name,
            old: Some(old.as_str()),
            new: None,
            via: WriteVia::Direct,
        };
        live_write(
            pool,
            repo_id,
            actor,
            Some(&write),
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        let reference = git::branch_ref(name).ok_or_else(|| super::invalid_ref("branch"))?;
        git::publish_ref_delete(self.path(), &reference, &old)
            .await
            .map_err(RepoError::from)?;
        record_ref_change(pool, repo_id).await;
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Tag publication has explicit authority and identity inputs."
    )]
    pub(super) async fn create_tag(
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
        if message.is_some_and(|message| message.len() > git::MAX_TAG_MESSAGE_BYTES) {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "The tag message is too large",
            ));
        }
        let _lock = git::locks::lock(self.path()).await;
        let reference = git::tag_ref(name).ok_or_else(|| super::invalid_ref("tag"))?;
        self.ensure_room_for_ref(&reference).await?;
        let (owned_name, owned_sha, owned_message) =
            (name.to_owned(), sha.to_owned(), message.map(str::to_owned));
        let (owned_tagger, owned_email) = (tagger_name.to_owned(), tagger_email.to_owned());
        let tag = self
            .blocking(move |repo| {
                git::prepare_create_tag(
                    repo,
                    &owned_name,
                    &owned_sha,
                    owned_message.as_deref(),
                    &owned_tagger,
                    &owned_email,
                )
            })
            .await?;
        live_write(
            pool,
            repo_id,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        git::publish_ref_create(self.path(), &reference, &tag.sha)
            .await
            .map_err(RepoError::from)?;
        record_ref_change(pool, repo_id).await;
        Ok(tag)
    }

    pub(super) async fn delete_tag(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        name: &str,
    ) -> Result<(), RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        let owned_name = name.to_owned();
        let old = self
            .blocking(move |repo| git::prepare_delete_tag(repo, &owned_name))
            .await?;
        live_write(
            pool,
            repo_id,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        let reference = git::tag_ref(name).ok_or_else(|| super::invalid_ref("tag"))?;
        git::publish_ref_delete(self.path(), &reference, &old)
            .await
            .map_err(RepoError::from)?;
        record_ref_change(pool, repo_id).await;
        Ok(())
    }

    pub(super) async fn prepare_file(
        &self,
        write: &FileWrite,
        content: &[u8],
        author: &str,
        email: &str,
    ) -> Result<editor::PreparedFile, RepoError> {
        editor::prepare(self.path(), write, content, author, email).await
    }

    pub(super) async fn prepare_upload(
        &self,
        write: &editor::UploadWrite,
        content: &[u8],
        author: &str,
        email: &str,
    ) -> Result<editor::PreparedFile, RepoError> {
        editor::prepare_upload(self.path(), write, content, author, email).await
    }

    pub(super) async fn publish_file(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        prepared: &editor::PreparedFile,
    ) -> Result<CommitInfo, RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        if prepared.expected.is_none() {
            // The save creates the branch.
            self.ensure_room_for_ref(&prepared.reference).await?;
        }
        let write = BranchWrite {
            branch: &prepared.branch,
            old: prepared.expected.as_deref(),
            new: Some(prepared.commit.sha.as_str()),
            via: WriteVia::Direct,
        };
        // The editor's commit has the expected tip as its only parent, or none, so it adds no
        // merge commit.
        live_write(
            pool,
            repo_id,
            actor,
            Some(&write),
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        let commit = editor::publish(self.path(), prepared).await?;
        record_branch_update(
            pool,
            repo_id,
            actor,
            self.path(),
            &prepared.branch,
            prepared.expected.as_deref(),
            &prepared.commit.sha,
        )
        .await;
        let defaults = self.synchronize_default_branch(pool, repo_id).await;
        record_ref_change(pool, repo_id).await;
        defaults?;
        Ok(commit)
    }

    /// Git has already published the branch. A metadata failure must not imply rollback.
    async fn synchronize_default_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
    ) -> Result<(), RepoError> {
        let result: anyhow::Result<()> = async {
            if let Some(branch) = git::repository::default_branch(self.path()).await? {
                self.set_head_target(&format!("refs/heads/{branch}"))
                    .await?;
                let repository = db::repositories::records::get_repository_by_id(pool, repo_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Repository disappeared after publication"))?;
                if repository.default_branch != branch {
                    db::repositories::records::update_repo_default_branch_and_size(
                        pool,
                        repo_id,
                        &branch,
                        repository.size_bytes,
                    )
                    .await?;
                }
            }
            Ok(())
        }
        .await;
        result.map_err(|error| {
            tracing::error!(repo_id, %error, "could not synchronize the default branch after publication");
            RepoError::applied_followup_failed()
        })
    }

    pub(super) async fn prepare_merge(&self, request: &MergeRequest) -> Result<String, RepoError> {
        let path = self.path();
        let committer = git::Person {
            name: &request.name,
            email: &request.email,
            date: None,
        };
        let merged = match request.method {
            MergeMethod::Merge => {
                git::prepare_merge(
                    path,
                    &request.head,
                    &request.base,
                    &request.message,
                    &committer,
                )
                .await
            }
            MergeMethod::Squash => {
                let author = match (&request.author_name, &request.author_email) {
                    (Some(name), Some(email)) => git::Person {
                        name: name.as_str(),
                        email: email.as_str(),
                        date: None,
                    },
                    _ => git::Person {
                        name: &request.name,
                        email: &request.email,
                        date: None,
                    },
                };
                git::prepare_squash(
                    path,
                    &request.head,
                    &request.base,
                    &request.message,
                    &author,
                    &committer,
                )
                .await
            }
            MergeMethod::Rebase => git::replay_commits(
                path,
                &request.base,
                &request.head,
                &request.base,
                &committer,
            )
            .await
            .and_then(|replayed| {
                if replayed.replayed == 0 {
                    Err(git::UnsupportedMerge(git::NO_CHANGES.to_owned()).into())
                } else {
                    Ok(replayed.tip)
                }
            }),
        };
        merged.map_err(RepoError::from)
    }

    /// Moves `base_branch` from `base` to `result`, provided every head in `heads` is still at the
    /// commit it was prepared from. Each head is `(head branch or pull ref, commit)`.
    #[expect(
        clippy::too_many_arguments,
        reason = "Merge publication carries explicit authority and CAS inputs."
    )]
    pub(super) async fn publish_merge(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        heads: &[(&str, &str)],
        base_branch: &str,
        base: &str,
        result: &str,
        method: MergeMethod,
    ) -> Result<(), RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        let write = BranchWrite {
            branch: base_branch,
            old: Some(base),
            new: Some(result),
            via: WriteVia::Merge,
        };
        let repository = live_write(
            pool,
            repo_id,
            actor,
            Some(&write),
            &History::Stored {
                repository: self.path(),
            },
            self.path(),
            self.node_id,
        )
        .await?;
        // The method may have been turned off after the merge was prepared. The record read under
        // the lock decides.
        if !allowed_merge_methods(&repository).contains(&method) {
            return Err(RepoError::merge_method_not_allowed());
        }
        git::publish_merge(self.path(), heads, base_branch, base, result)
            .await
            .map_err(RepoError::from)?;
        record_branch_update(
            pool,
            repo_id,
            actor,
            self.path(),
            base_branch,
            Some(base),
            result,
        )
        .await;
        record_ref_change(pool, repo_id).await;
        Ok(())
    }

    /// Replays the commits of `from..head` onto `onto`, for a restack. No branch moves.
    pub(super) async fn prepare_restack(
        &self,
        from: &str,
        head: &str,
        onto: &str,
        name: &str,
        email: &str,
    ) -> Result<String, RepoError> {
        let committer = git::Person {
            name,
            email,
            date: None,
        };
        git::replay_commits(self.path(), from, head, onto, &committer)
            .await
            .map(|replayed| replayed.tip)
            .map_err(RepoError::from)
    }

    /// Moves each branch in `moves` after checking, under the repository lock, the actor's write
    /// access and each branch's protection rules. A restack force-updates a head branch, which
    /// the rules that block force pushes refuse.
    pub(super) async fn publish_restack(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        base_branch: &str,
        base: &str,
        moves: &[RestackMove],
    ) -> Result<(), RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        live_write(
            pool,
            repo_id,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        for restack in moves {
            let write = BranchWrite {
                branch: &restack.branch,
                old: Some(&restack.old),
                new: Some(&restack.new),
                via: WriteVia::Direct,
            };
            live_write(
                pool,
                repo_id,
                actor,
                Some(&write),
                &History::Stored {
                    repository: self.path(),
                },
                self.path(),
                self.node_id,
            )
            .await?;
        }
        let ref_moves: Vec<(&str, &str, &str)> = moves
            .iter()
            .map(|restack| {
                (
                    restack.branch.as_str(),
                    restack.old.as_str(),
                    restack.new.as_str(),
                )
            })
            .collect();
        git::publish_restack(self.path(), base_branch, base, &ref_moves)
            .await
            .map_err(RepoError::from)?;
        for restack in moves {
            record_branch_update(
                pool,
                repo_id,
                actor,
                self.path(),
                &restack.branch,
                Some(restack.old.as_str()),
                &restack.new,
            )
            .await;
        }
        record_ref_change(pool, repo_id).await;
        Ok(())
    }

    // ----- Git sessions -----

    pub(super) async fn advertise_refs(&self, service: GitService) -> Result<Vec<u8>, RepoError> {
        let _permit = git::repository::process::session_permit()
            .await
            .map_err(RepoError::from)?;
        git::advertise_service_refs(self.path(), service.name())
            .await
            .map_err(RepoError::from)
    }

    /// Runs one `git` command against this repository, with the usual scrubbed
    /// environment and limits.
    async fn git(&self, args: &[&str]) -> Result<(std::process::ExitStatus, Vec<u8>), RepoError> {
        let mut command = process::git_command().map_err(RepoError::from)?;
        command.arg("--git-dir").arg(self.path()).args(args);
        process::buffered(command, b"", MAX_GIT_OUTPUT, process::RPC_TIMEOUT)
            .await
            .map_err(RepoError::from)
    }

    /// The branch HEAD names, even when it is unborn; `None` when HEAD is detached.
    pub(super) async fn head_target(&self) -> Result<Option<String>, RepoError> {
        let (status, output) = self.git(&["symbolic-ref", "--quiet", "HEAD"]).await?;
        if !status.success() {
            return Ok(None);
        }
        let target = String::from_utf8(output).map_err(|_| unavailable("HEAD is not text"))?;
        Ok(Some(target.trim().to_owned()))
    }

    /// Streams a pack of the objects that the listed refs need beyond the tips a copy
    /// holds. The copy sets its refs from the listing sent with this request, so every
    /// target has to be covered by the pack.
    pub(super) async fn replica_pack(
        &self,
        targets: &[String],
        have: &[String],
        streams: super::GitStreams<'_>,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let mut revs = String::new();
        for target in targets {
            anyhow::ensure!(is_object_id(target), "invalid replica ref target");
            revs.push_str(target);
            revs.push('\n');
        }
        for tip in have {
            anyhow::ensure!(is_object_id(tip), "invalid replica tip");
            revs.push('^');
            revs.push_str(tip);
            revs.push('\n');
        }
        let mut input = revs.as_bytes();
        let mut command = process::git_command()?;
        command
            .arg("--git-dir")
            .arg(self.path())
            .args(["pack-objects", "--revs", "--stdout"]);
        process::stream(command, &mut input, streams.output, streams.errors, false).await
    }

    /// Stores a pack from the primary in this repository. Git reads the whole pack, so a
    /// truncated transfer fails here and leaves the refs alone.
    pub(super) async fn index_pack(&self, pack: &Path) -> Result<(), RepoError> {
        let mut input = tokio::fs::File::open(pack)
            .await
            .map_err(|_| unavailable("Git could not read the replica pack"))?;
        let mut command = process::git_command().map_err(RepoError::from)?;
        command
            .arg("--git-dir")
            .arg(self.path())
            .args(["index-pack", "--stdin"]);
        let status = process::stream(
            command,
            &mut input,
            &mut tokio::io::sink(),
            &mut tokio::io::sink(),
            false,
        )
        .await
        .map_err(RepoError::from)?;
        if !status.success() {
            return Err(unavailable("Git could not store the replica pack"));
        }
        Ok(())
    }

    /// Makes this copy's refs exactly `refs`, in one ref transaction. The objects they
    /// name must already be stored.
    pub(super) async fn set_refs(&self, refs: &HashMap<String, String>) -> Result<(), RepoError> {
        let current = self.mirror_refs().await?;
        let mut script = String::from("start\n");
        for (name, sha) in refs {
            if !name.starts_with("refs/") || !git2::Reference::is_valid_name(name) {
                return Err(RepoError::new(
                    ErrorKind::Invalid,
                    "invalid replica ref name",
                ));
            }
            if !is_object_id(sha) {
                return Err(RepoError::new(
                    ErrorKind::Invalid,
                    "invalid replica ref value",
                ));
            }
            script.push_str(&format!("update {name} {sha}\n"));
        }
        for name in current.keys().filter(|name| !refs.contains_key(*name)) {
            script.push_str(&format!("delete {name}\n"));
        }
        script.push_str("prepare\ncommit\n");
        let mut command = process::git_command().map_err(RepoError::from)?;
        command
            .arg("--git-dir")
            .arg(self.path())
            .args(["update-ref", "--stdin"]);
        let (status, _) = process::buffered(
            command,
            script.as_bytes(),
            MAX_GIT_OUTPUT,
            process::RPC_TIMEOUT,
        )
        .await
        .map_err(RepoError::from)?;
        if !status.success() {
            return Err(unavailable("Git could not set the replica's refs"));
        }
        Ok(())
    }

    /// Makes an existing branch the default. The database is updated first, so an archived
    /// repository refuses the change before HEAD moves.
    pub(super) async fn set_default_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        name: &str,
    ) -> Result<(), RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        let reference = git::branch_ref(name).ok_or_else(|| super::invalid_ref("branch"))?;
        let (status, _) = self
            .git(&["show-ref", "--verify", "--quiet", &reference])
            .await?;
        if !status.success() {
            return Err(RepoError::new(
                ErrorKind::NotFound,
                "That branch does not exist",
            ));
        }
        let updated = db::repositories::records::set_repository_default_branch(pool, repo_id, name)
            .await
            .map_err(|_| unavailable("Repository storage is unavailable"))?;
        if !updated {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "This repository is archived and read-only.",
            ));
        }
        self.set_head_target(&reference).await?;
        // Replicas copy HEAD with the repository, so the change counts as a write.
        record_ref_change(pool, repo_id).await;
        // The stored languages describe the default branch, so store the new one's.
        languages::refresh_default_branch(
            pool.clone(),
            repo_id,
            self.path().to_owned(),
            name.to_owned(),
        );
        Ok(())
    }

    /// Points HEAD at a branch, such as `refs/heads/main`.
    pub(super) async fn set_head_target(&self, target: &str) -> Result<(), RepoError> {
        if !target.starts_with("refs/heads/") || !git2::Reference::is_valid_name(target) {
            return Err(RepoError::new(ErrorKind::Invalid, "invalid HEAD target"));
        }
        let (status, _) = self.git(&["symbolic-ref", "HEAD", target]).await?;
        if !status.success() {
            return Err(unavailable("Git could not set HEAD"));
        }
        Ok(())
    }

    pub(super) async fn git_session(
        &self,
        session: super::GitSession,
        receive: Option<(
            crate::backend::db::DbPool,
            std::sync::Arc<crate::backend::ci::CiRuns>,
        )>,
        streams: super::GitStreams<'_>,
    ) -> anyhow::Result<std::process::ExitStatus> {
        use crate::backend::{db, repositories::git_sessions};
        if session.service == GitService::ReceivePack {
            let (pool, ci) =
                receive.ok_or_else(|| anyhow::anyhow!("receive resources required"))?;
            let uuid = super::rpc::repository_uuid(&self.disk_path)?;
            let repo = db::cluster::get_repository_by_uuid(&pool, &uuid.to_string())
                .await?
                .ok_or_else(|| anyhow::anyhow!("repository unavailable"))?;
            let result = git_sessions::receive_session(
                pool.clone(),
                ci,
                git_sessions::ReceiveSession {
                    repo_id: repo.id,
                    path: self.path(),
                    session,
                },
                streams,
            )
            .await;
            return result;
        }
        let mut command = process::git_command()?;
        command.arg("upload-pack");
        if session.stateless {
            command.arg("--stateless-rpc");
        }
        if let Some(protocol) = session.git_protocol {
            command.env("GIT_PROTOCOL", protocol);
        }
        command.arg(self.path());
        process::stream(
            command,
            streams.input,
            streams.output,
            streams.errors,
            false,
        )
        .await
    }
}
