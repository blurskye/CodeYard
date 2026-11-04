//! The local store: the bare repository is a directory on this machine. Each
//! method runs the matching Git code, moving blocking libgit2 work off the async
//! threads and turning failures into [`RepoError`]s.
mod branches;
mod gists;
use super::{
    allowed_merge_methods, editor, AsyncMergePublication, BlobBytes, BlobPreviewMetadata,
    BlobRange, FileWrite, GitService, MergeMethod, MergeRequest, RestackMove,
    MAX_ARCHIVE_RESPONSE_BYTES, MAX_BLOB_RESPONSE_BYTES,
};
use crate::backend::{
    db,
    repositories::{
        access, languages,
        protection::{check_branch_write, BranchWrite, History, Refusal, WriteVia},
    },
};
use codeyard_git::error::{ErrorKind, RepoError};
use codeyard_git::{
    self as git, error::MERGE_COMMIT_BLOCKED, repository::process, BlobContent, BranchStatus,
    CiCommit, CiWorkflowFile, CiWorkflowSource, CommitComparison, CommitFilter, CommitInfo,
    ContributorInfo, DiffInfo, FileBlame, FileList, LanguageStat, RepoInfo, TagInfo, TreeEntry,
    WeekChanges,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use uuid::Uuid;

mod exports;
mod imports;
mod writes;
use writes::oid_change;

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
    /// old primary afterwards. `Some(None)` expects an unplaced repository;
    /// `None` is reserved for internal stores opened from a bare path.
    node_id: Option<Option<i64>>,
    context: Option<(crate::backend::cluster::NodeContext, i64)>,
    term: Arc<tokio::sync::OnceCell<(i64, PathBuf)>>,
}

fn unavailable(message: &'static str) -> RepoError {
    RepoError::new(ErrorKind::Unavailable, message)
}

async fn require_legacy_publisher(pool: &db::DbPool, repo: i64) -> Result<(), RepoError> {
    crate::backend::cluster::writes::require_legacy_publisher(pool, repo)
        .await
        .map_err(|error| {
            if error.is::<crate::backend::cluster::writes::PublisherUnavailable>() {
                RepoError::new(ErrorKind::Unavailable, error.to_string())
            } else {
                RepoError::authority_unavailable()
            }
        })
}

async fn record_secret_bypasses(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    bypasses: Vec<crate::backend::repositories::secret_scanning::push_protection::ApprovedBypass>,
) -> Result<(), RepoError> {
    let mut failed = false;
    for bypass in bypasses {
        if let Err(error) =
            db::secret_scanning::use_bypass(pool, repo_id, actor, &bypass.hash, chrono::Utc::now())
                .await
        {
            tracing::warn!(repo_id, %error, "accepted publication bypass could not be recorded");
            failed = true;
        }
    }
    if failed {
        Err(RepoError::applied_followup_failed())
    } else {
        Ok(())
    }
}

fn normalize_blob_range(range: BlobRange, size: u64) -> Result<(u64, usize), RepoError> {
    if matches!(range, BlobRange::Preview { .. }) {
        return Err(RepoError::new(
            ErrorKind::Invalid,
            "Preview requires a stream reader",
        ));
    }
    let (offset, length) = range
        .normalize(size)
        .map_err(|_| RepoError::range_not_satisfiable())?;
    let length = usize::try_from(length).map_err(|_| RepoError::response_too_large())?;
    if length > MAX_BLOB_RESPONSE_BYTES {
        return Err(RepoError::response_too_large());
    }
    Ok((offset, length))
}

fn repository_matches_path(
    repository: &db::models::Repository,
    path: &Path,
    node_id: Option<Option<i64>>,
) -> bool {
    if node_id.is_some_and(|expected| repository.storage_node_id != expected) {
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

async fn live_pull_read(
    pool: &db::DbPool,
    repo_id: i64,
    actor: super::PullHeadActor,
    path: &Path,
    node_id: Option<Option<i64>>,
) -> Result<(), RepoError> {
    if let Some(token) = actor.token {
        let user = db::users::tokens::token_user(pool, token, false)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            .filter(|user| user.id == actor.user)
            .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "repository read denied"))?;
        if let Some(bounds) = crate::backend::identity::github_apps::authority(pool, token)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
        {
            if !bounds
                .allows(pool, user.id, repo_id, "contents", false)
                .await
                .map_err(|_| RepoError::authority_unavailable())?
            {
                return Err(RepoError::new(ErrorKind::Invalid, "repository read denied"));
            }
        } else if user.username == db::ci_tokens::BOT {
            let job = db::ci_tokens::get(pool, token)
                .await
                .map_err(|_| RepoError::authority_unavailable())?
                .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "repository read denied"))?;
            let permissions: crate::backend::ci::tokens::Permissions =
                serde_json::from_str(&job.permissions)
                    .map_err(|_| RepoError::authority_unavailable())?;
            if job.repo_id != repo_id || !permissions.allows("contents", false) {
                return Err(RepoError::new(ErrorKind::Invalid, "repository read denied"));
            }
        } else if db::fine_grained_tokens::classic_refused_repository(pool, token, repo_id)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
        {
            return Err(RepoError::new(ErrorKind::Invalid, "repository read denied"));
        }
    }
    live_read(pool, repo_id, actor.user, path, node_id).await
}

async fn live_read(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    path: &Path,
    node_id: Option<Option<i64>>,
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
    if !access::can_read_git(pool, Some(&user), &repository)
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
    node_id: Option<Option<i64>>,
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
    checked_branch_write(pool, &repository, &user, branch, history).await?;
    Ok(repository)
}

async fn checked_branch_write(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    user: &db::models::User,
    branch: Option<&BranchWrite<'_>>,
    history: &History<'_>,
) -> Result<(), RepoError> {
    if let Some(write) = branch {
        match check_branch_write(pool, repository, user, write, history).await {
            Ok(_) => {}
            Err(Refusal::Blocked(MERGE_COMMIT_BLOCKED)) => {
                return Err(RepoError::merge_commit_blocked())
            }
            Err(Refusal::Blocked(reason)) => return Err(RepoError::protected_refusal(reason)),
            Err(Refusal::Unavailable) => return Err(RepoError::authority_unavailable()),
        }
    }
    Ok(())
}

async fn live_pull_head_write(
    pool: &db::DbPool,
    (head_id, base_id, actor): (i64, i64, i64),
    request: &super::PullBranchUpdate,
    target: &LocalStore,
    branch: Option<&BranchWrite<'_>>,
    history: &History<'_>,
) -> Result<db::models::Repository, RepoError> {
    let user = db::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| RepoError::authority_unavailable())?
        .ok_or_else(RepoError::write_denied)?;
    let head = access::current_repository(pool, head_id)
        .await
        .map_err(|error| match error {
            access::AccessError::Unavailable => RepoError::authority_unavailable(),
            _ => RepoError::write_denied(),
        })?;
    let base = access::current_repository(pool, base_id)
        .await
        .map_err(|error| match error {
            access::AccessError::Unavailable => RepoError::authority_unavailable(),
            _ => RepoError::write_denied(),
        })?;
    let pull = db::pull_requests::records::get_pull_request_by_id(pool, request.pull_request_id)
        .await
        .map_err(|_| RepoError::authority_unavailable())?
        .ok_or_else(|| RepoError::new(ErrorKind::Conflict, "Pull request changed"))?;
    if pull.repo_id != base_id
        || pull.head_repo_id != Some(head_id)
        || pull.head_branch != request.branch
        || pull.base_branch != request.base_branch
        || pull.state != "open"
    {
        return Err(RepoError::new(ErrorKind::Conflict, "Pull request changed"));
    }
    if !repository_matches_path(&head, target.path(), target.node_id)
        || !crate::backend::repositories::pull_requests::maintainer_edit::can_update_head(
            pool, &user, &base, &head, &pull,
        )
        .await
        .map_err(|_| RepoError::authority_unavailable())?
    {
        return Err(RepoError::write_denied());
    }
    if let Some(token) = request.token {
        for (repo, write) in [(base_id, false), (head_id, true)] {
            if !crate::backend::identity::tokens::repository_scope(
                pool, token, actor, repo, "contents", write,
            )
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            {
                return Err(RepoError::write_denied());
            }
        }
    }
    checked_branch_write(pool, &head, &user, branch, history).await?;
    Ok(head)
}

/// Records a branch update made by the web, the API or a merge as profile activity, and
/// refreshes the stored languages when the branch is the default branch. Git has already moved
/// the branch, so a failure is logged rather than reported as a failed write.
async fn record_branch_update(
    pool: &db::DbPool,
    repo_id: i64,
    actor: i64,
    storage: (&Path, Option<Option<i64>>),
    branch: &str,
    old: Option<&str>,
    new: &str,
) {
    let (path, node_id) = storage;
    languages::refresh_default_branch(
        pool.clone(),
        repo_id,
        path.to_owned(),
        branch.to_owned(),
        node_id,
    );
    let reference = format!("refs/heads/{branch}");
    let count = match git::count_pushed_commits(path, old, new, &reference).await {
        Ok(count) => i32::try_from(count).unwrap_or(i32::MAX),
        Err(error) => {
            tracing::warn!(repo_id, %error, "could not count a branch update");
            return;
        }
    };
    // This receipt also owns pushed-commit totals, not just optional feed decoration.
    // Let database admission finish instead of cancelling a valid insert under contention.
    if let Err(error) =
        db::activity::record_repository_push(pool, repo_id, actor, branch, count).await
    {
        tracing::warn!(repo_id, %error, "could not record a branch update");
    }
}

/// Ref changes in progress on this node, counted per repository directory from before the
/// database counts each one until Git has made it. A primary serves no replica copy while a
/// change to that repository is counted, because the copy would claim a version whose change
/// it may not hold yet.
static REF_CHANGES: LazyLock<Mutex<HashMap<PathBuf, usize>>> = LazyLock::new(Default::default);

type SizeResult = Option<(u64, Result<i64, RepoError>)>;
struct SizeRefresh {
    requested: u64,
    completed: tokio::sync::watch::Sender<SizeResult>,
}
static SIZE_REFRESHES: LazyLock<Mutex<HashMap<PathBuf, SizeRefresh>>> =
    LazyLock::new(Default::default);

/// Coalesce writes while a scan runs. Accounting never holds a publication lock or RefChange.
pub(super) fn refresh_storage_size(pool: db::DbPool, repo_id: i64, store: LocalStore) {
    request_storage_size(pool, repo_id, store);
}

pub(super) async fn refresh_storage_size_now(
    pool: db::DbPool,
    repo_id: i64,
    store: LocalStore,
) -> Result<i64, RepoError> {
    let (ticket, mut completed) = request_storage_size(pool, repo_id, store);
    tokio::time::timeout(process::RPC_TIMEOUT, async {
        loop {
            if let Some((generation, result)) = completed.borrow_and_update().clone() {
                if generation >= ticket {
                    return result;
                }
            }
            completed.changed().await.map_err(|_| {
                RepoError::new(ErrorKind::Unavailable, "Repository accounting stopped")
            })?;
        }
    })
    .await
    .map_err(|_| RepoError::new(ErrorKind::Unavailable, "Repository accounting timed out"))?
}

fn request_storage_size(
    pool: db::DbPool,
    repo_id: i64,
    store: LocalStore,
) -> (u64, tokio::sync::watch::Receiver<SizeResult>) {
    let path = store.path().to_owned();
    let receiver = {
        let mut pending = SIZE_REFRESHES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(refresh) = pending.get_mut(&path) {
            refresh.requested += 1;
            return (refresh.requested, refresh.completed.subscribe());
        }
        let (completed, receiver) = tokio::sync::watch::channel(None);
        pending.insert(
            path.clone(),
            SizeRefresh {
                requested: 1,
                completed,
            },
        );
        receiver
    };
    tokio::spawn(async move {
        loop {
            let generation =
                SIZE_REFRESHES.lock().unwrap_or_else(|e| e.into_inner())[&path].requested;
            // One owner scans and publishes in order, without retaining a publication lease.
            drop(git::locks::lock(&path).await);
            let result = async {
                let size = store.size_bytes().await?.min(i64::MAX as u64) as i64;
                match store.node_id {
                    Some(node) => {
                        db::repositories::records::update_repo_size_at(&pool, repo_id, size, node)
                            .await
                    }
                    None => db::repositories::records::update_repo_size(&pool, repo_id, size).await,
                }
                .map_err(RepoError::from)?;
                Ok(size)
            }
            .await;
            if let Err(error) = &result {
                tracing::warn!(repo_id, %error, "could not refresh repository storage size");
            }
            let mut pending = SIZE_REFRESHES.lock().unwrap_or_else(|e| e.into_inner());
            let refresh = pending.get_mut(&path).expect("owned size refresh");
            refresh.completed.send_replace(Some((generation, result)));
            if refresh.requested == generation {
                pending.remove(&path);
                break;
            }
        }
    });
    (1, receiver)
}

fn ref_change_counts() -> MutexGuard<'static, HashMap<PathBuf, usize>> {
    REF_CHANGES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// The key that counts a repository directory. Every path to one directory gives the same key.
async fn counting_key(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_owned())
}

/// A ref change that Git is about to make, counted until it drops. Hold it until the change
/// has been made or has failed.
pub struct RefChange {
    key: PathBuf,
}

impl Drop for RefChange {
    fn drop(&mut self) {
        let mut counts = ref_change_counts();
        if let Some(count) = counts.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

/// Starts a ref change on the repository at `path`, before Git makes it. The change is counted
/// in this process, then the database counts it, but only while `node_id` holds the repository;
/// `Some(None)` expects no placement; `None` skips the check for an internal bare path.
/// A promotion that reads the
/// version after this sees the new version and refuses a copy that is behind it. A copy that
/// asks for its pack while the change is counted is refused, so it never claims a version whose
/// change it lacks. If the change is refused, nothing is counted.
pub async fn begin_ref_change(
    pool: &db::DbPool,
    repo_id: i64,
    path: &Path,
    node_id: Option<Option<i64>>,
) -> Result<RefChange, RepoError> {
    require_legacy_publisher(pool, repo_id).await?;
    let change = count_ref_change(path).await;
    match db::cluster::bump_write_version(pool, repo_id, node_id).await {
        Ok(true) => Ok(change),
        Ok(false) => Err(RepoError::write_denied()),
        Err(error) => {
            tracing::error!(repo_id, %error, "could not count a ref change for replicas");
            Err(unavailable("Repository storage is unavailable."))
        }
    }
}

async fn count_ref_change(path: &Path) -> RefChange {
    let key = counting_key(path).await;
    *ref_change_counts().entry(key.clone()).or_default() += 1;
    RefChange { key }
}

/// Only publishers which use the coordinator may skip the legacy replication counter.
pub(crate) async fn begin_coordinated_ref_change(
    pool: &db::DbPool,
    repo_id: i64,
    path: &Path,
    node_id: Option<Option<i64>>,
) -> Result<RefChange, RepoError> {
    let repository = db::repositories::records::get_repository_by_id(pool, repo_id)
        .await
        .map_err(|_| RepoError::authority_unavailable())?
        .ok_or_else(RepoError::write_denied)?;
    if repository.storage_node_id.is_some()
        && db::cluster::writes::write_settings(pool)
            .await
            .map_err(RepoError::from)?
            .write_quorum
            .is_some()
    {
        crate::backend::cluster::context::check_term_directory(
            pool,
            &repository,
            node_id.flatten(),
            path,
        )
        .await?;
        return Ok(count_ref_change(path).await);
    }
    begin_ref_change(pool, repo_id, path, node_id).await
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
        git::MAX_PULL_HEAD_REFS,
        git::MAX_PULL_HEAD_REF_BYTES,
    )
}

/// Reconstructs a non-serving tenure without moving or modifying its predecessor.
/// Missing retained objects leave the repository fenced rather than guessing at refs.
pub(super) async fn recover_term(
    pool: &db::DbPool,
    repository: i64,
    node: i64,
    root: &Path,
) -> anyhow::Result<()> {
    use crate::backend::cluster::{context::term_path, writes};
    use anyhow::{ensure, Context};
    let view = db::cluster::writes::recovery_view(pool, repository).await?;
    if view.serving || !view.voters.iter().any(|voter| voter.node_id == node) {
        return Ok(());
    }
    let row = db::repositories::records::get_repository_by_id(pool, repository)
        .await?
        .context("repository was deleted during recovery")?;
    let uuid = super::rpc::repository_uuid(&row.disk_path)?;
    let target = term_path(root, uuid, view.term);
    ensure!(
        view.refs.iter().any(|value| value.name == "HEAD"),
        "canonical HEAD is missing"
    );
    let _lock = git::locks::lock(&target).await;
    let _readers = git::locks::exclusive(&target).await;
    let view = writes::recover(pool, repository, view.primary_id, view.term).await?;
    let target_store = LocalStore::new(target.to_str().context("invalid term path")?, None);
    if !tokio::fs::try_exists(&target).await? {
        let previous = term_path(root, uuid, view.term - 1);
        let source = if tokio::fs::try_exists(&previous).await? {
            crate::backend::cluster::context::literal_directory(root, &previous).await?;
            previous
        } else if view.term == 1 {
            let primary = super::primary_path(root, uuid);
            let mirror = crate::backend::cluster::replicas::copy_path(root, uuid);
            if tokio::fs::try_exists(&primary).await? {
                primary
            } else {
                mirror
            }
        } else {
            anyhow::bail!("previous term objects are unavailable; repository remains fenced");
        };
        let parent = target.parent().context("invalid term path")?;
        tokio::fs::create_dir_all(parent).await?;
        crate::backend::cluster::context::literal_directory(root, parent).await?;
        let stage = tempfile::tempdir_in(parent)?;
        let source = std::path::absolute(source)?;
        let source_store = LocalStore::new(source.to_str().context("invalid source path")?, None);
        source_store.mirror_refs().await?;
        let staged = LocalStore::new(stage.path().to_str().context("invalid staging path")?, None);
        staged.initialize(None, "main").await?;
        // Fetch uses an owned, bounded Git child rather than copying files in a
        // blocking task that could outlive the staging directory on cancellation.
        let (status, _) = staged
            .git(&[
                "-c",
                "transfer.hideRefs=!refs/codeyard/",
                "fetch",
                "--quiet",
                "--no-write-fetch-head",
                "--no-tags",
                "--",
                source.to_str().context("invalid source path")?,
                "+refs/*:refs/*",
            ])
            .await?;
        ensure!(
            status.success(),
            "retained objects are unavailable; repository remains fenced"
        );
        let current = staged.mirror_refs().await?;
        let mut remove = String::from("start\n");
        for name in current.keys() {
            remove.push_str(&format!("delete {name}\n"));
        }
        remove.push_str("prepare\ncommit\n");
        staged.recovery_refs(&remove).await?;
        let mut install = String::from("start\n");
        for value in &view.refs {
            if value.kind == "oid" {
                install.push_str(&format!("update {} {}\n", value.name, value.target));
            }
        }
        install.push_str("prepare\ncommit\n");
        staged.recovery_refs(&install).await?;
        for value in &view.refs {
            if value.kind == "symbolic" {
                let (status, _) = staged
                    .git(&["symbolic-ref", &value.name, &value.target])
                    .await?;
                ensure!(status.success(), "could not restore canonical symbolic ref");
            }
        }
        staged.verify_canonical(&view.refs).await?;
        let current = db::cluster::writes::recovery_view(pool, repository).await?;
        ensure!(
            !current.serving
                && current.term == view.term
                && current.committed_version == view.committed_version,
            "recovery term changed"
        );
        tokio::fs::rename(stage.path(), &target).await?;
        let parent = parent.to_owned();
        let root = root.to_owned();
        process::blocking_until(Instant::now() + process::RPC_TIMEOUT, move || {
            for directory in parent.ancestors() {
                std::fs::File::open(directory)?.sync_all()?;
                if directory == root {
                    break;
                }
            }
            Ok(())
        })
        .await?;
    }
    crate::backend::cluster::context::literal_directory(root, &target).await?;
    target_store.verify_canonical(&view.refs).await?;
    ensure!(
        db::cluster::writes::mark_write_applied(
            pool,
            repository,
            node,
            view.term,
            view.committed_version
        )
        .await?,
        "recovery term changed"
    );
    if let Some(write) = &view.unfinished {
        // A committed operation is released only once both reconstructed copies are durable.
        db::cluster::writes::finish_write(pool, &write.id, view.term).await?;
    }
    Ok(())
}

impl LocalStore {
    async fn recovery_refs(&self, script: &str) -> anyhow::Result<()> {
        let mut command = process::git_command()?;
        command
            .arg("--git-dir")
            .arg(self.path())
            .args(["update-ref", "--no-deref", "--stdin"]);
        let (status, _) = process::buffered(
            command,
            script.as_bytes(),
            MAX_GIT_OUTPUT,
            process::RPC_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(status.success(), "could not restore canonical refs");
        Ok(())
    }

    async fn verify_canonical(&self, values: &[db::models::RefValue]) -> anyhow::Result<()> {
        let values = values.to_vec();
        self.blocking(move |path| {
            let refs = git::list_mirror_refs(path)?;
            anyhow::ensure!(
                refs.len() == values.iter().filter(|value| value.name != "HEAD").count(),
                "unexpected term refs"
            );
            let repo = git2::Repository::open_bare(path)?;
            for value in values {
                let reference = repo.find_reference(&value.name)?;
                let matches = if value.kind == "symbolic" {
                    reference.symbolic_target() == Some(value.target.as_str())
                } else {
                    reference.symbolic_target().is_none()
                        && reference.target().map(|oid| oid.to_string()).as_deref()
                            == Some(value.target.as_str())
                };
                anyhow::ensure!(matches, "term refs differ from canonical refs");
            }
            Ok(())
        })
        .await?;
        let (status, _) = self
            .git(&[
                "fsck",
                "--strict",
                "--full",
                "--no-dangling",
                "--no-reflogs",
                "--no-progress",
            ])
            .await?;
        anyhow::ensure!(
            status.success(),
            "canonical objects are incomplete or corrupt"
        );
        Ok(())
    }

    pub(super) fn new(disk_path: &str, node_id: Option<Option<i64>>) -> Self {
        Self {
            disk_path: disk_path.to_owned(),
            node_id,
            context: None,
            term: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    pub(super) fn with_context(
        mut self,
        context: &crate::backend::cluster::NodeContext,
        repository: i64,
    ) -> Self {
        self.context = Some((context.clone(), repository));
        self
    }

    pub(super) async fn admitted_path(&self, read_only: bool) -> Result<PathBuf, RepoError> {
        let Some((context, repository)) = &self.context else {
            return Ok(self.path().to_owned());
        };
        let uuid = super::rpc::repository_uuid(&self.disk_path)?;
        let (path, term) = context
            .repository_path(
                *repository,
                uuid,
                read_only,
                self.term.get().map(|(term, _)| *term),
            )
            .await?;
        if let Some(term) = term {
            let bound = self
                .term
                .get_or_init(|| async { (term, path.clone()) })
                .await;
            if bound.0 != term || bound.1 != path {
                return Err(unavailable("Repository term is no longer serving"));
            }
        }
        Ok(path)
    }

    pub(super) fn disk_path(&self) -> &str {
        &self.disk_path
    }

    pub(super) fn path(&self) -> &Path {
        self.term
            .get()
            .map_or_else(|| Path::new(&self.disk_path), |(_, path)| path.as_path())
    }

    /// Runs blocking Git code against this repository on a worker thread.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, RepoError> {
        self.blocking_until(Instant::now() + process::RPC_TIMEOUT, work)
            .await
    }

    async fn blocking_until<T: Send + 'static>(
        &self,
        deadline: Instant,
        work: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, RepoError> {
        let path = self.admitted_path(true).await?;
        process::blocking_until(deadline, move || work(&path))
            .await
            .map_err(RepoError::from)
    }

    /// Refuses a new branch or tag once the repository is at the limit.
    async fn ensure_room_for_ref(&self, reference: &str) -> Result<(), RepoError> {
        let held = self.blocking(git::published_refs_in_use).await?;
        ensure_ref_room(held, &[reference])
    }

    pub(super) async fn branch_publication(
        &self,
        repo_id: i64,
        reference: &str,
    ) -> Result<Option<git::PublicationRecord>, RepoError> {
        self.admitted_path(true).await?;
        let _lock = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let reference = reference.to_owned();
        self.blocking(move |path| {
            let _held = _lock;
            git::read_branch_publication(path, repo_id, &reference)
        })
        .await
    }

    async fn begin_branch_change(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        writes: &[&BranchWrite<'_>],
        publication: &Arc<git::locks::PublicationGuard>,
        prepared_repository: Option<&Path>,
    ) -> Result<Arc<RefChange>, RepoError> {
        let (repository, user) =
            access::check_write(pool, repo_id, actor)
                .await
                .map_err(|error| match error {
                    access::AccessError::Unavailable => RepoError::authority_unavailable(),
                    _ => RepoError::write_denied(),
                })?;
        if !repository_matches_path(&repository, self.path(), self.node_id) {
            return Err(RepoError::write_denied());
        }
        let updates = writes
            .iter()
            .map(|write| {
                Ok(git::PublicationUpdate {
                    reference: git::branch_ref(write.branch)
                        .ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?,
                    old: write.old.map(str::to_owned),
                    new: write.new.map(str::to_owned),
                })
            })
            .collect::<Result<Vec<_>, RepoError>>()?;
        let change = Arc::new(self.begin_publisher_change(pool, repo_id).await?);
        if !updates.is_empty() {
            self.prepare_publications(
                pool,
                repo_id,
                git::PublicationActor::User(actor),
                &user.username,
                &updates,
                publication,
                &change,
                async {
                    for write in writes {
                        live_write(
                            pool,
                            repo_id,
                            actor,
                            Some(*write),
                            &History::Stored {
                                repository: prepared_repository.unwrap_or(self.path()),
                            },
                            self.path(),
                            self.node_id,
                        )
                        .await?;
                    }
                    Ok(())
                },
            )
            .await?;
        }
        // Preparing a large push can take time. Its intent is not permission to publish after
        // access or branch policy changed while the metadata objects were being written.
        // A cross-repository update's objects remain in its prepared repository until import.
        let history = History::Stored {
            repository: prepared_repository.unwrap_or(self.path()),
        };
        for write in writes {
            live_write(
                pool,
                repo_id,
                actor,
                Some(write),
                &history,
                self.path(),
                self.node_id,
            )
            .await?;
        }
        Ok(change)
    }

    /// Recheck the initiating contents credential on a write that publishes no ref yet.
    async fn check_contents_grant(
        &self,
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        token: Option<i64>,
    ) -> Result<(), RepoError> {
        let Some(token) = token else { return Ok(()) };
        if !db::users::tokens::repository_write(pool, token, actor, repo, "contents")
            .await
            .map_err(|_| RepoError::authority_unavailable())?
        {
            return Err(RepoError::write_denied());
        }
        Ok(())
    }

    /// Recheck the initiating contents credential at publication. Without workflows permission,
    /// updates must also leave workflow files as they are or as an existing branch has them.
    /// The contents check runs last, after the possibly slow tree inspection, so a credential
    /// revoked meanwhile refuses the ref update that follows.
    async fn check_publication_grant(
        &self,
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        token: Option<i64>,
        updates: Vec<(String, String)>,
        quarantine: Option<PathBuf>,
    ) -> Result<(), RepoError> {
        let Some(token) = token else {
            return Ok(());
        };
        if !crate::backend::identity::oauth_apps::allows_workflows(pool, token)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
        {
            self.blocking(move |path| {
                git::oauth::check_workflows(path, quarantine.as_deref(), &updates)
            })
            .await
            .map_err(|_| RepoError::workflow_denied())?;
        }
        self.check_contents_grant(pool, repo, actor, Some(token))
            .await
    }

    pub(super) async fn git_tree(
        &self,
        revision: &str,
        recursive: bool,
    ) -> Result<git::objects::TreeObject, RepoError> {
        git::objects::read_tree(self.path(), revision, recursive)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn introduced_blobs(
        &self,
        sha: &str,
    ) -> Result<Vec<git::objects::TreeObjectEntry>, RepoError> {
        git::objects::introduced_blobs(self.path(), sha)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn git_object_kind(&self, sha: &str) -> Result<String, RepoError> {
        let sha = sha.to_owned();
        self.blocking(move |path| git::objects::object_kind(path, &sha))
            .await
    }

    pub(super) async fn git_tag(&self, sha: &str) -> Result<git::objects::TagObject, RepoError> {
        let sha = sha.to_owned();
        self.blocking(move |path| git::objects::read_tag(path, &sha))
            .await
    }

    pub(super) async fn write_git_object(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        token: Option<i64>,
        input: git::objects::ObjectWrite,
        bytes: &[u8],
    ) -> Result<String, RepoError> {
        let limit = if matches!(input, git::objects::ObjectWrite::Blob) {
            git::objects::MAX_BLOB_WRITE_BYTES
        } else {
            0
        };
        if bytes.len() > limit {
            return Err(RepoError::response_too_large());
        }
        self.admitted_path(false).await?;
        // The publication guard includes the process lock; taking both would wait on itself.
        let lock = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let permit = process::operation_slot(process::RPC_TIMEOUT)
            .await
            .map_err(|_| unavailable("Git operation timed out"))?;
        let repository = live_write(
            pool,
            repo_id,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        self.check_contents_grant(pool, repo_id, actor, token)
            .await?;
        let bytes = bytes.to_vec();
        let path = self.path().to_owned();
        let held = lock.clone();
        let sha = tokio::task::spawn_blocking(move || {
            let _held = (held, permit);
            git::objects::write(&path, input, &bytes)
        })
        .await
        .map_err(|_| unavailable("Git did not finish"))?
        .map_err(RepoError::from)?;
        // An object no ref names is not in any replica pack. In a replicated repository a
        // hidden ref keeps it, and the ref change makes replicas copy it like any other
        // accepted write. A single server keeps it until Git prunes unreachable objects.
        let reference = format!("refs/codeyard/objects/{sha}");
        if repository.storage_node_id.is_some() {
            let retained_ref = reference.clone();
            if !self
                .blocking(move |path| git::repository::retained::exists(path, &retained_ref))
                .await?
            {
                let held = self.blocking(git::retained_objects_in_use).await?;
                if ensure_room(
                    held,
                    &[&reference],
                    "Git-data objects that no branch or tag names",
                    git::MAX_RETAINED_OBJECT_REFS,
                    git::MAX_RETAINED_OBJECT_REF_BYTES,
                )
                .is_err()
                {
                    let candidates = self.blocking(git::repository::retained::candidates).await?;
                    let redundant = git::repository::retained::reachable(self.path(), candidates)
                        .await
                        .map_err(RepoError::from)?;
                    if !redundant.is_empty() {
                        let _change = self.begin_publisher_change(pool, repo_id).await?;
                        let changes = redundant
                            .iter()
                            .map(|(name, oid)| oid_change(name, Some(oid), None))
                            .collect::<Vec<_>>();
                        if !lock
                            .scope(self.publish_quorum_refs(pool, repo_id, &changes, async {
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
                                self.check_contents_grant(pool, repo_id, actor, token).await
                            }))
                            .await?
                        {
                            git::repository::retained::delete(self.path(), &redundant)
                                .await
                                .map_err(RepoError::from)?;
                        }
                    }
                    ensure_room(
                        self.blocking(git::retained_objects_in_use).await?,
                        &[&reference],
                        "Git-data objects that no branch or tag names",
                        git::MAX_RETAINED_OBJECT_REFS,
                        git::MAX_RETAINED_OBJECT_REF_BYTES,
                    )?;
                }
                let _change = self.begin_publisher_change(pool, repo_id).await?;
                let changes = [oid_change(&reference, None, Some(&sha))];
                if !lock
                    .scope(self.publish_quorum_refs(pool, repo_id, &changes, async {
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
                        self.check_contents_grant(pool, repo_id, actor, token).await
                    }))
                    .await?
                {
                    git::publish_ref_create(self.path(), &reference, &sha)
                        .await
                        .map_err(RepoError::from)?;
                }
            }
        }
        refresh_storage_size(pool.clone(), repo_id, self.clone());
        Ok(sha)
    }

    // ----- Opening and lifecycle -----

    pub(super) async fn create(
        repos_root: &Path,
        default_branch: &str,
        source: Option<&LocalStore>,
    ) -> Result<Self, RepoError> {
        let refused = || unavailable("Repository storage is unavailable.");
        let root = std::path::absolute(repos_root).map_err(|_| refused())?;
        let disk_path = super::primary_path(&root, Uuid::new_v4())
            .to_str()
            .ok_or_else(refused)?
            .to_owned();
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|_| refused())?;
        let store = Self::new(&disk_path, None);
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
        if let Some((context, repository)) = &self.context {
            require_legacy_publisher(context.database()?, *repository).await?;
        }
        let source = match source {
            Some(source) => Some(source.admitted_path(true).await?),
            None => None,
        };
        git::initialize_repository(self.path(), source.as_deref(), default_branch)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn initialize_copy(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        source_repo_id: i64,
        authority: db::credentials::Actor,
        source: &LocalStore,
        options: &super::CopyOptions,
    ) -> Result<(), RepoError> {
        let actor = authority.user_id;
        source.admitted_path(true).await?;
        let default_branch = options.default_branch.as_str();
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
        git::initialize_repository_copy(
            temporary.path(),
            Some(source.path()),
            default_branch,
            options.default_branch_only,
            options.template,
        )
        .await
        .map_err(RepoError::from)?;
        if options.template {
            let user = db::users::accounts::get_user_by_id(pool, actor)
                .await
                .map_err(RepoError::from)?
                .ok_or_else(RepoError::write_denied)?;
            let email = user
                .email
                .unwrap_or_else(|| format!("{}@users.noreply", user.username));
            git::rewrite_template_history(
                temporary.path(),
                default_branch,
                (&user.username, &email),
            )
            .await
            .map_err(RepoError::from)?;
        }
        live_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
        if options.template
            && !db::repositories::records::get_repository_by_id(pool, source_repo_id)
                .await
                .map_err(RepoError::from)?
                .is_some_and(|repo| repo.is_template)
        {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Source is not a template",
            ));
        }
        let _publication = git::locks::lock(self.path()).await;
        // A pending gist has no ready gist row yet; its explicit owner is its only writer.
        // Ordinary copies recheck destination authority after potentially slow copying.
        let destination = access::current_repository(pool, repo_id)
            .await
            .map_err(|error| match error {
                access::AccessError::Unavailable => RepoError::authority_unavailable(),
                _ => RepoError::write_denied(),
            })?;
        if !repository_matches_path(&destination, self.path(), self.node_id) {
            return Err(RepoError::write_denied());
        }
        if destination.is_gist {
            if destination.owner_user_id != Some(actor) || destination.is_archived {
                return Err(RepoError::write_denied());
            }
        } else {
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
        }
        let copied = Self::new(
            temporary
                .path()
                .to_str()
                .ok_or_else(|| unavailable("Invalid copy path"))?,
            None,
        )
        .refs()
        .await?;
        let owner = db::repositories::records::repository_owner_names(pool, &[repo_id])
            .await
            .map_err(RepoError::from)?
            .remove(&repo_id)
            .ok_or_else(|| unavailable("Repository owner unavailable"))?;
        crate::backend::repositories::releases::immutable::check_namespace(
            pool,
            &destination,
            &owner,
            &destination.name,
            &copied,
        )
        .await?;
        // Retain the initiating credential through copying, lock waits and namespace checks.
        live_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
        if let Some(token_id) = authority.token_id() {
            let source_repository = access::current_repository(pool, source_repo_id)
                .await
                .map_err(|_| RepoError::authority_unavailable())?;
            crate::backend::repositories::git_sessions::ReceiveCredential::Token(token_id)
                .authorize(pool, &source_repository, false)
                .await
                .map_err(RepoError::from)?;
        }
        if !destination.is_gist {
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
        }
        access::credential_user(pool, &authority, true)
            .await
            .map_err(RepoError::from)?;
        require_legacy_publisher(pool, repo_id).await?;
        tokio::fs::rename(temporary.path(), self.path())
            .await
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        if !db::cluster::bump_write_version(pool, repo_id, self.node_id)
            .await
            .map_err(RepoError::from)?
        {
            return Err(RepoError::write_denied());
        }
        languages::refresh_default_branch(
            pool.clone(),
            repo_id,
            self.path().to_owned(),
            default_branch.to_owned(),
            self.node_id,
        );
        Ok(())
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

    pub(super) async fn retire(&self) -> Result<(), RepoError> {
        self.blocking(git::mirror::retire).await
    }

    pub(super) async fn exists(&self) -> bool {
        self.blocking(|path| Ok(git::is_git_repo(path)))
            .await
            .unwrap_or(false)
    }

    pub(super) async fn size_bytes(&self) -> Result<u64, RepoError> {
        self.blocking(codeyard_git::stats::directory_size).await
    }

    // ----- Reads -----

    pub(super) async fn refs(&self) -> Result<HashMap<String, String>, RepoError> {
        self.blocking(git::list_refs).await
    }

    pub(super) async fn matching_refs(
        &self,
        prefix: &str,
    ) -> Result<HashMap<String, String>, RepoError> {
        let prefix = prefix.to_owned();
        self.blocking(move |path| git::list_matching_refs(path, &prefix))
            .await
    }

    /// Every ref, for a replica to copy.
    pub(super) async fn mirror_refs(&self) -> Result<HashMap<String, String>, RepoError> {
        self.blocking(git::list_mirror_refs).await
    }

    pub(super) async fn info(&self) -> Result<RepoInfo, RepoError> {
        git::get_repo_info(&self.admitted_path(true).await?)
            .await
            .map_err(RepoError::from)
    }

    /// The advertised default branch alone, for callers that already hold the refs.
    pub(super) async fn default_branch(&self) -> Result<String, RepoError> {
        let branch = git::repository::default_branch(&self.admitted_path(true).await?)
            .await
            .map_err(RepoError::from)?;
        Ok(branch.unwrap_or_else(|| "main".into()))
    }

    pub(super) async fn commit_count(&self) -> Result<u64, RepoError> {
        git::count_commits(&self.admitted_path(true).await?)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn tree(
        &self,
        git_ref: &str,
        path: &str,
        metadata_only: bool,
    ) -> Result<Vec<TreeEntry>, RepoError> {
        let directory = self.admitted_path(true).await?;
        if metadata_only {
            git::list_tree_with_history(&directory, git_ref, path, false, true).await
        } else {
            git::list_tree(&directory, git_ref, path).await
        }
        .map_err(RepoError::from)
    }

    pub(super) async fn sidebar_trees(
        &self,
        git_ref: &str,
        paths: &[String],
    ) -> Result<Vec<(String, Vec<TreeEntry>)>, RepoError> {
        git::repository::list_sidebar_trees(&self.admitted_path(true).await?, git_ref, paths)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn file_paths(
        &self,
        git_ref: &str,
        limit: usize,
        query: &str,
    ) -> Result<FileList, RepoError> {
        git::list_file_paths(&self.admitted_path(true).await?, git_ref, limit, query)
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

    pub(super) async fn template_texts(
        &self,
        revision: &str,
        paths: &[String],
    ) -> Result<Vec<Option<(String, String)>>, RepoError> {
        git::repository::template_texts(&self.admitted_path(true).await?, revision, paths)
            .await
            .map_err(RepoError::from)
    }

    async fn read_blob_object_range(
        &self,
        id: &str,
        size: u64,
        mode: u32,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        let directory = self.admitted_path(true).await?;
        if let BlobRange::Preview { offset, line } = range {
            // The preview reader starts its own `git cat-file`, so it takes a slot like any other Git work.
            let _permit =
                git::repository::process::operation_slot(git::repository::process::RPC_TIMEOUT)
                    .await
                    .map_err(|_| unavailable("Git operation timed out"))?;
            let preview = git::repository::read_blob_preview(&directory, id, size, offset, line)
                .await
                .map_err(RepoError::from)?;
            return Ok(BlobBytes {
                id: id.to_owned(),
                bytes: preview.bytes,
                is_binary: preview.is_binary,
                size,
                offset: preview.offset,
                mode,
                preview: Some(BlobPreviewMetadata {
                    first_line: preview.first_line,
                    previous: preview.previous,
                }),
            });
        }
        let (offset, length) = normalize_blob_range(range, size)?;
        let (bytes, is_binary) = git::read_blob_range(&directory, id, size, offset, length)
            .await
            .map_err(RepoError::from)?;
        Ok(BlobBytes {
            id: id.to_owned(),
            bytes,
            is_binary,
            size,
            offset,
            mode,
            preview: None,
        })
    }

    pub(super) async fn blob(&self, git_ref: &str, path: &str) -> Result<BlobContent, RepoError> {
        let (owned_ref, owned_path) = (git_ref.to_owned(), path.to_owned());
        let (id, size, mode) = self
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
        let blob = self.read_blob_object_range(&id, size, mode, range).await?;
        let content = if blob.is_binary {
            format!("[Binary file, {} bytes]", size)
        } else {
            if size > MAX_BLOB_RESPONSE_BYTES as u64 {
                return Err(RepoError::response_too_large());
            }
            // Invalid bytes become three-byte replacement characters, so check the result.
            let text = String::from_utf8_lossy(&blob.bytes);
            if text.len() > MAX_BLOB_RESPONSE_BYTES {
                return Err(RepoError::response_too_large());
            }
            text.into_owned()
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
        let (id, size, mode) = self
            .blocking(move |repo| git::blob_metadata(repo, &git_ref, &path))
            .await?;
        self.read_blob_object_range(&id, size, mode, range).await
    }

    pub(super) async fn blob_by_id_range(
        &self,
        id: &str,
        range: BlobRange,
    ) -> Result<BlobBytes, RepoError> {
        let oid = id.to_owned();
        let size = self
            .blocking(move |path| {
                let repo = git2::Repository::open_bare(path)?;
                let (size, kind) = repo.odb()?.read_header(git2::Oid::from_str(&oid)?)?;
                anyhow::ensure!(kind == git2::ObjectType::Blob, "Not a blob");
                Ok(size as u64)
            })
            .await?;
        self.read_blob_object_range(id, size, 0o100644, range).await
    }

    pub(super) async fn blob_by_id(&self, id: &str) -> Result<Vec<u8>, RepoError> {
        git::get_blob_by_id(&self.admitted_path(true).await?, id)
            .await
            .map_err(RepoError::from)
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
        self.blocking_until(deadline, move |repo| {
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
        git::search_code(
            &self.admitted_path(true).await?,
            git_ref,
            text,
            offset,
            limit,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn pull_request_commits(
        &self,
        base: &str,
        head: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(u64, Vec<CommitInfo>), RepoError> {
        git::pull_request_commits(&self.admitted_path(true).await?, base, head, offset, limit)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn commits_between(
        &self,
        base_ref: &str,
        head_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        git::range_commits(
            &self.admitted_path(true).await?,
            base_ref,
            head_ref,
            offset,
            limit,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn release_history(
        &self,
        head: &str,
        previous: Option<&str>,
        exclude_tag: &str,
    ) -> Result<git::ReleaseHistory, RepoError> {
        git::release_history(
            &self.admitted_path(true).await?,
            head,
            previous,
            exclude_tag,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn path_commits(
        &self,
        git_ref: &str,
        path: &str,
        offset: usize,
        limit: usize,
        filter: &CommitFilter,
    ) -> Result<Vec<CommitInfo>, RepoError> {
        git::list_path_commits(
            &self.admitted_path(true).await?,
            git_ref,
            path,
            offset,
            limit,
            filter,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn branch_statuses(
        &self,
        base: &str,
        heads: &[String],
    ) -> Result<Vec<BranchStatus>, RepoError> {
        git::branch_statuses(
            &self.admitted_path(true).await?,
            base,
            heads,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn commit(&self, revision: &str) -> Result<CommitInfo, RepoError> {
        let revision = revision.to_owned();
        self.blocking(move |repo| git::get_commit(repo, &revision))
            .await
    }

    pub(super) async fn commit_diff(&self, revision: &str) -> Result<DiffInfo, RepoError> {
        git::get_commit_diff(&self.admitted_path(true).await?, revision)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn diff(&self, base_ref: &str, head_ref: &str) -> Result<DiffInfo, RepoError> {
        git::get_diff(&self.admitted_path(true).await?, base_ref, head_ref)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn diff_stats(
        &self,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<DiffInfo, RepoError> {
        git::get_diff_stats(&self.admitted_path(true).await?, base_ref, head_ref)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn diff_file(
        &self,
        base_ref: &str,
        head_ref: &str,
        path: &str,
    ) -> Result<DiffInfo, RepoError> {
        git::get_file_diff(&self.admitted_path(true).await?, base_ref, head_ref, path)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn changed_paths(
        &self,
        base_ref: &str,
        head_ref: &str,
        endpoints: bool,
        renames: bool,
    ) -> Result<Vec<String>, RepoError> {
        git::changed_paths(
            &self.admitted_path(true).await?,
            base_ref,
            head_ref,
            endpoints,
            renames,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn diff_from(
        &self,
        source: &LocalStore,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<DiffInfo, RepoError> {
        git::compare_repositories(
            &self.admitted_path(true).await?,
            &source.admitted_path(true).await?,
            base_ref,
            head_ref,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn comparison_from(
        &self,
        source: &LocalStore,
        base_ref: &str,
        head_ref: &str,
        offset: usize,
        limit: usize,
    ) -> Result<CommitComparison, RepoError> {
        git::compare_commits(
            &self.admitted_path(true).await?,
            &source.admitted_path(true).await?,
            base_ref,
            head_ref,
            offset,
            limit,
        )
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
            &self.admitted_path(true).await?,
            revision,
            prefix,
            format,
            MAX_ARCHIVE_RESPONSE_BYTES,
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn languages(&self, git_ref: &str) -> Result<Vec<LanguageStat>, RepoError> {
        git::compute_language_stats(&self.admitted_path(true).await?, git_ref)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn blame(&self, git_ref: &str, path: &str) -> Result<FileBlame, RepoError> {
        git::blame_file(&self.admitted_path(true).await?, git_ref, path)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn branch_contributors(
        &self,
        branch: &str,
    ) -> Result<Vec<ContributorInfo>, RepoError> {
        git::get_branch_contributors(&self.admitted_path(true).await?, branch)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn contributors(&self) -> Result<Vec<ContributorInfo>, RepoError> {
        git::get_contributors(&self.admitted_path(true).await?)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn commit_statistics(
        &self,
        branch: &str,
        mode: git::StatisticsMode,
    ) -> Result<git::CommitStatistics, RepoError> {
        git::get_commit_statistics(&self.admitted_path(true).await?, branch, mode)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn code_frequency(&self, branch: &str) -> Result<Vec<WeekChanges>, RepoError> {
        git::get_code_frequency(&self.admitted_path(true).await?, branch)
            .await
            .map_err(RepoError::from)
    }

    pub(super) async fn is_ancestor(
        &self,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, RepoError> {
        git::commit_is_ancestor(&self.admitted_path(true).await?, ancestor, descendant)
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
        git::checkout(&self.admitted_path(true).await?, destination, sha)
            .await
            .map_err(RepoError::from)
    }

    /// Materialize a remote upload-pack response on the calling CI node.
    pub(super) async fn checkout_pack(
        destination: &Path,
        sha: &str,
        source: &str,
        pack: tempfile::NamedTempFile,
        permit: tokio::sync::OwnedSemaphorePermit,
        lock: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), RepoError> {
        let destination = destination.to_owned();
        let sha = sha.to_owned();
        let source = source.to_owned();
        tokio::spawn(async move {
            let (_permit, _lock) = (permit, lock);
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut input = tokio::fs::File::from_std(pack.reopen().map_err(anyhow::Error::from)?);
            input.rewind().await.map_err(anyhow::Error::from)?;
            let mut header = [0; 8];
            input
                .read_exact(&mut header)
                .await
                .map_err(anyhow::Error::from)?;
            if &header != b"0008NAK\n" {
                return Err(unavailable("Invalid upload-pack response"));
            }
            let result =
                git::repository::materialize_checkout(&destination, &sha, &source, Some(input))
                    .await
                    .map_err(RepoError::from);
            if result.is_err() {
                let _ = tokio::fs::remove_dir_all(&destination).await;
            }
            result
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
        actor: impl Into<super::PullHeadActor>,
        source: &LocalStore,
        number: i32,
        head: &str,
        expected_head: Option<&str>,
    ) -> Result<(), RepoError> {
        let actor = actor.into();
        self.admitted_path(false).await?;
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

        live_pull_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
        live_pull_read(pool, repo_id, actor, self.path(), self.node_id).await?;
        source.admitted_path(true).await?;
        let prepared = git::repository::prepare_pull_head(self.path(), source.path(), head)
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
        actor: impl Into<super::PullHeadActor>,
        source: &LocalStore,
        number: i32,
        head: &str,
        expected_head: Option<&str>,
        prepared: git::repository::PreparedPullHead,
    ) -> Result<(), RepoError> {
        let actor = actor.into();
        self.admitted_path(false).await?;
        source.admitted_path(true).await?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
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
        live_pull_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
        live_pull_read(pool, repo_id, actor, self.path(), self.node_id).await?;
        // An export locks the repository under this same lock before it copies the Git data, so
        // a head published into a locked repository would be missing from the archive.
        if db::repository_exports::locked(pool, repo_id)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
        {
            return Err(RepoError::write_denied());
        }
        let _change = self.begin_publisher_change(pool, repo_id).await?;
        self.import_prepared_objects(pool, repo_id, prepared.path(), &[head.to_owned()])
            .await?;
        let changes = [oid_change(
            &format!("refs/pull/{number}/head"),
            expected_head,
            Some(head),
        )];
        if !publication
            .scope(self.publish_quorum_refs(pool, repo_id, &changes, async {
                live_pull_read(pool, source_repo_id, actor, source.path(), source.node_id).await?;
                live_pull_read(pool, repo_id, actor, self.path(), self.node_id).await?;
                if db::repository_exports::locked(pool, repo_id)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                {
                    return Err(RepoError::write_denied());
                }
                Ok(())
            }))
            .await?
        {
            publication
                .scope(git::repository::publish_pull_head(
                    self.path(),
                    number,
                    head,
                    expected_head,
                    &prepared,
                ))
                .await
                .map_err(RepoError::from)?;
        }
        refresh_storage_size(pool.clone(), repo_id, self.clone());
        git::maintenance::after_push(repo_id, self.path().to_owned());
        Ok(())
    }

    pub(super) async fn create_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        name: &str,
        sha: &str,
    ) -> Result<String, RepoError> {
        self.admitted_path(false).await?;
        let actor = authority.user_id;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let reference =
            git::branch_ref(name).ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?;
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
        let bypasses =
            crate::backend::repositories::secret_scanning::push_protection::refuse_blocked_secrets(
                pool,
                &repository,
                actor,
                self.path(),
                None,
                std::slice::from_ref(&target),
            )
            .await
            .map_err(RepoError::from)?;
        let _change = self
            .begin_branch_change(pool, repo_id, actor, &[&write], &publication, None)
            .await?;
        access::check_publication_actor(pool, &authority, &repository)
            .await
            .map_err(RepoError::from)?;
        let changes = [oid_change(&reference, None, Some(&target))];
        if !publication
            .scope(self.publish_quorum_refs(
                pool,
                repo_id,
                &changes,
                self.check_ref_publication(
                    pool,
                    repo_id,
                    &authority,
                    &reference,
                    None,
                    Some(&target),
                ),
            ))
            .await?
        {
            publication
                .scope(git::publish_ref_create(self.path(), &reference, &target))
                .await
                .map_err(RepoError::from)?;
        }
        let followup = self
            .after_ref_change(
                pool,
                repo_id,
                actor,
                &[(reference, editor::ZERO.to_owned(), target.clone())],
            )
            .await;
        record_branch_update(
            pool,
            repo_id,
            actor,
            (self.path(), self.node_id),
            name,
            None,
            &target,
        )
        .await;
        let bypass_followup = record_secret_bypasses(pool, repo_id, actor, bypasses).await;
        followup?;
        bypass_followup?;
        Ok(target)
    }

    pub(super) async fn update_reference(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        reference: &str,
        sha: &str,
        force: bool,
    ) -> Result<(String, String), RepoError> {
        self.admitted_path(false).await?;
        let actor = authority.user_id;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let (owned_reference, owned_sha) = (reference.to_owned(), sha.to_owned());
        let (old, new, commits) = self
            .blocking(move |path| git::objects::ref_target(path, &owned_reference, &owned_sha))
            .await?;
        if !force
            && old != new
            && (!commits
                || !git::commit_is_ancestor(self.path(), &old, &new)
                    .await
                    .map_err(RepoError::from)?)
        {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Update is not a fast forward",
            ));
        }
        let branch = reference.strip_prefix("refs/heads/");
        let write = branch.map(|branch| BranchWrite {
            branch,
            old: Some(&old),
            new: Some(&new),
            via: WriteVia::Direct,
        });
        let repository = live_write(
            pool,
            repo_id,
            actor,
            write.as_ref(),
            &History::Stored {
                repository: self.path(),
            },
            self.path(),
            self.node_id,
        )
        .await?;
        crate::backend::repositories::releases::immutable::check_current_tag_updates(
            pool,
            &repository,
            &[
                crate::backend::repositories::git_sessions::validation::RefUpdate {
                    reference: reference.to_owned(),
                    old: old.clone(),
                    new: new.clone(),
                },
            ],
        )
        .await?;
        if reference.starts_with("refs/tags/") {
            let replacement = (reference.to_owned(), new.clone());
            self.blocking(move |path| git::check_tag_budget(path, None, &[replacement]))
                .await?;
        }
        access::check_publication_actor(pool, &authority, &repository)
            .await
            .map_err(RepoError::from)?;
        if old == new {
            return Ok((old, new));
        }
        let bypasses =
            crate::backend::repositories::secret_scanning::push_protection::refuse_blocked_secrets(
                pool,
                &repository,
                actor,
                self.path(),
                None,
                std::slice::from_ref(&new),
            )
            .await
            .map_err(RepoError::from)?;
        let writes = write.as_ref().into_iter().collect::<Vec<_>>();
        let _change = self
            .begin_branch_change(pool, repo_id, actor, &writes, &publication, None)
            .await?;
        access::check_publication_actor(pool, &authority, &repository)
            .await
            .map_err(RepoError::from)?;
        let changes = [oid_change(reference, Some(&old), Some(&new))];
        if !publication
            .scope(self.publish_quorum_refs(
                pool,
                repo_id,
                &changes,
                self.check_ref_publication(
                    pool,
                    repo_id,
                    &authority,
                    reference,
                    Some(&old),
                    Some(&new),
                ),
            ))
            .await?
        {
            publication
                .scope(git::publish_ref(
                    self.path(),
                    "update reference",
                    reference,
                    Some(&new),
                    &old,
                ))
                .await
                .map_err(RepoError::from)?;
        }
        let followup = self
            .after_ref_change(
                pool,
                repo_id,
                actor,
                &[(reference.to_owned(), old.clone(), new.clone())],
            )
            .await;
        if let Some(branch) = branch {
            record_branch_update(
                pool,
                repo_id,
                actor,
                (self.path(), self.node_id),
                branch,
                Some(&old),
                &new,
            )
            .await;
        }
        let bypass_followup = record_secret_bypasses(pool, repo_id, actor, bypasses).await;
        followup?;
        bypass_followup?;
        Ok((old, new))
    }

    pub(super) async fn delete_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        name: &str,
    ) -> Result<(), RepoError> {
        self.admitted_path(false).await?;
        let actor = authority.user_id;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
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
        let repository = live_write(
            pool,
            repo_id,
            actor,
            Some(&write),
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        let reference =
            git::branch_ref(name).ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?;
        let _change = self
            .begin_branch_change(pool, repo_id, actor, &[&write], &publication, None)
            .await?;
        access::check_publication_actor(pool, &authority, &repository)
            .await
            .map_err(RepoError::from)?;
        let changes = [oid_change(&reference, Some(&old), None)];
        if !publication
            .scope(self.publish_quorum_refs(
                pool,
                repo_id,
                &changes,
                self.check_ref_publication(pool, repo_id, &authority, &reference, Some(&old), None),
            ))
            .await?
        {
            publication
                .scope(git::publish_ref_delete(self.path(), &reference, &old))
                .await
                .map_err(RepoError::from)?;
        }
        self.after_ref_change(
            pool,
            repo_id,
            actor,
            &[(reference, old, editor::ZERO.to_owned())],
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Tag publication has explicit authority and identity inputs."
    )]
    pub(super) async fn create_tag(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        name: &str,
        sha: &str,
        message: Option<&str>,
        tagger_name: &str,
        tagger_email: &str,
    ) -> Result<TagInfo, RepoError> {
        self.admitted_path(false).await?;
        let actor = authority.user_id;
        if message.is_some_and(|message| message.len() > git::MAX_TAG_MESSAGE_BYTES) {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "The tag message is too large",
            ));
        }
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let reference =
            git::tag_ref(name).ok_or_else(|| codeyard_git::error::invalid_ref("tag"))?;
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
        let repository = live_write(
            pool,
            repo_id,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        crate::backend::repositories::releases::immutable::check_current_tag_updates(
            pool,
            &repository,
            &[
                crate::backend::repositories::git_sessions::validation::RefUpdate {
                    reference: reference.clone(),
                    old: editor::ZERO.to_owned(),
                    new: tag.sha.clone(),
                },
            ],
        )
        .await?;
        let replacement = (reference.clone(), tag.sha.clone());
        self.blocking(move |path| git::check_tag_budget(path, None, &[replacement]))
            .await?;
        let bypasses =
            crate::backend::repositories::secret_scanning::push_protection::refuse_blocked_secrets(
                pool,
                &repository,
                actor,
                self.path(),
                None,
                std::slice::from_ref(&tag.sha),
            )
            .await
            .map_err(RepoError::from)?;
        let _change = self.begin_publisher_change(pool, repo_id).await?;
        access::check_publication_actor(pool, &authority, &repository)
            .await
            .map_err(RepoError::from)?;
        let changes = [oid_change(&reference, None, Some(&tag.sha))];
        if !publication
            .scope(self.publish_quorum_refs(
                pool,
                repo_id,
                &changes,
                self.check_ref_publication(
                    pool,
                    repo_id,
                    &authority,
                    &reference,
                    None,
                    Some(&tag.sha),
                ),
            ))
            .await?
        {
            publication
                .scope(git::publish_ref_create(self.path(), &reference, &tag.sha))
                .await
                .map_err(RepoError::from)?;
        }
        let followup = self
            .after_ref_change(
                pool,
                repo_id,
                actor,
                &[(reference, editor::ZERO.to_owned(), tag.sha.clone())],
            )
            .await;
        let bypass_followup = record_secret_bypasses(pool, repo_id, actor, bypasses).await;
        followup?;
        bypass_followup?;
        Ok(tag)
    }

    pub(super) async fn delete_tag(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        name: &str,
    ) -> Result<(), RepoError> {
        self.admitted_path(false).await?;
        let actor = authority.user_id;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let owned_name = name.to_owned();
        let old = self
            .blocking(move |repo| git::prepare_delete_tag(repo, &owned_name))
            .await?;
        let repository = live_write(
            pool,
            repo_id,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        let reference =
            git::tag_ref(name).ok_or_else(|| codeyard_git::error::invalid_ref("tag"))?;
        crate::backend::repositories::releases::immutable::check_current_tag_updates(
            pool,
            &repository,
            &[
                crate::backend::repositories::git_sessions::validation::RefUpdate {
                    reference: reference.clone(),
                    old: old.clone(),
                    new: editor::ZERO.to_owned(),
                },
            ],
        )
        .await?;
        let _change = self.begin_publisher_change(pool, repo_id).await?;
        access::check_publication_actor(pool, &authority, &repository)
            .await
            .map_err(RepoError::from)?;
        let changes = [oid_change(&reference, Some(&old), None)];
        if !publication
            .scope(self.publish_quorum_refs(
                pool,
                repo_id,
                &changes,
                self.check_ref_publication(pool, repo_id, &authority, &reference, Some(&old), None),
            ))
            .await?
        {
            publication
                .scope(git::publish_ref_delete(self.path(), &reference, &old))
                .await
                .map_err(RepoError::from)?;
        }
        self.after_ref_change(
            pool,
            repo_id,
            actor,
            &[(reference, old, editor::ZERO.to_owned())],
        )
        .await
    }

    pub(super) async fn prepare_file(
        &self,
        write: &FileWrite,
        content: &[u8],
        author: &str,
        email: &str,
    ) -> Result<editor::PreparedFile, RepoError> {
        editor::prepare(
            &self.admitted_path(true).await?,
            write,
            content,
            author,
            email,
        )
        .await
    }

    pub(super) async fn prepare_file_with_identities(
        &self,
        write: &FileWrite,
        content: &[u8],
        author: &str,
        email: &str,
        identities: &editor::CommitIdentities,
    ) -> Result<editor::PreparedFile, RepoError> {
        editor::prepare_with_identities(
            &self.admitted_path(true).await?,
            write,
            content,
            author,
            email,
            identities,
        )
        .await
    }

    pub(super) async fn prepare_upload(
        &self,
        write: &editor::UploadWrite,
        content: &[u8],
        author: &str,
        email: &str,
    ) -> Result<editor::PreparedFile, RepoError> {
        editor::prepare_upload(
            &self.admitted_path(true).await?,
            write,
            content,
            author,
            email,
        )
        .await
    }

    /// Boxed with an explicit `Send` bound for the same reason as `rename_branch`.
    pub(super) fn publish_file(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        prepared: &editor::PreparedFile,
        dependency_publication: Option<&db::repositories::dependencies::Publication>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CommitInfo, RepoError>> + Send>>
    {
        let (store, pool, prepared, dependency_publication) = (
            self.clone(),
            pool.clone(),
            prepared.clone(),
            dependency_publication.cloned(),
        );
        Box::pin(async move {
            tokio::spawn(async move {
                store
                    .publish_file_owned(
                        &pool,
                        repo_id,
                        actor,
                        &prepared,
                        dependency_publication.as_ref(),
                    )
                    .await
            })
            .await
            .map_err(|_| RepoError::applied_followup_failed())?
        })
    }

    async fn publish_file_owned(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        prepared: &editor::PreparedFile,
        dependency_publication: Option<&db::repositories::dependencies::Publication>,
    ) -> Result<CommitInfo, RepoError> {
        self.admitted_path(false).await?;
        let staged = editor::claim_staged(self.path(), prepared)?;
        let prepared_path = staged.as_ref().map_or(self.path(), |staged| staged.path());
        let quarantine = staged.as_ref().map(|staged| staged.path().join("objects"));
        if self.quorum_context(pool, repo_id).await?.is_some() {
            editor::validate_staged(self.path(), prepared, staged.as_deref()).await?;
        }
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
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
        // An update adds one prepared commit. A new branch must also satisfy rules for
        // the source history that it brings into the destination.
        let history = if prepared.new_branch {
            History::Stored {
                repository: prepared_path,
            }
        } else {
            History::SingleCommit
        };
        let repository = live_write(
            pool,
            repo_id,
            actor,
            Some(&write),
            &history,
            self.path(),
            self.node_id,
        )
        .await?;
        if prepared.web_commit {
            let settings =
                db::organizations::settings::repository_requires_web_signoff(pool, repo_id)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                    .ok_or_else(RepoError::write_denied)?;
            if settings
                && !db::organizations::settings::has_signoff(
                    &prepared.commit.message,
                    &prepared.commit.author_name,
                    &prepared.commit.author_email,
                )
            {
                return Err(RepoError::new(
                    ErrorKind::Invalid,
                    db::organizations::settings::required_signoff_message(
                        &prepared.commit.author_name,
                        &prepared.commit.author_email,
                    ),
                ));
            }
        }
        if let Some(update) = dependency_publication {
            crate::backend::repositories::dependencies::check_publication(
                actor,
                &repository,
                prepared,
                update,
                &self.refs().await?,
            )?;
            db::repositories::dependencies::plan_update(
                pool,
                &update.job,
                &update.key,
                &update.update,
            )
            .await
            .map_err(crate::backend::repositories::dependencies::store_error)?;
        }
        // A saved file is scanned like a push. It must not carry a secret that push protection
        // blocks, even though it never goes through receive-pack.
        let bypasses =
            crate::backend::repositories::secret_scanning::push_protection::refuse_blocked_secrets(
                pool,
                &repository,
                actor,
                self.path(),
                quarantine.as_deref(),
                std::slice::from_ref(&prepared.commit.sha),
            )
            .await
            .map_err(RepoError::from)?;
        let _change = self
            .begin_branch_change(
                pool,
                repo_id,
                actor,
                &[&write],
                &publication,
                Some(prepared_path),
            )
            .await?;
        if let Some(authority) = &prepared.authority {
            if authority.user_id != actor {
                return Err(RepoError::write_denied());
            }
            self.check_publication_grant(
                pool,
                repo_id,
                actor,
                authority.token_id(),
                vec![(
                    prepared
                        .expected
                        .clone()
                        .unwrap_or_else(|| editor::ZERO.to_owned()),
                    prepared.commit.sha.clone(),
                )],
                quarantine.clone(),
            )
            .await?;
            access::check_publication_actor(pool, authority, &repository).await?;
        }
        self.import_prepared_objects(
            pool,
            repo_id,
            prepared_path,
            std::slice::from_ref(&prepared.commit.sha),
        )
        .await?;
        let changes = [oid_change(
            &prepared.reference,
            prepared.expected.as_deref(),
            Some(&prepared.commit.sha),
        )];
        let quorum = publication
            .scope(self.publish_quorum_refs(pool, repo_id, &changes, async {
                let repository = live_write(
                    pool,
                    repo_id,
                    actor,
                    Some(&write),
                    &history,
                    self.path(),
                    self.node_id,
                )
                .await?;
                if let Some(authority) = &prepared.authority {
                    self.check_publication_grant(
                        pool,
                        repo_id,
                        actor,
                        authority.token_id(),
                        vec![(
                            prepared
                                .expected
                                .clone()
                                .unwrap_or_else(|| editor::ZERO.to_owned()),
                            prepared.commit.sha.clone(),
                        )],
                        quarantine.clone(),
                    )
                    .await?;
                    access::check_publication_actor(pool, authority, &repository).await?;
                }
                Ok(())
            }))
            .await?;
        let commit = if quorum {
            prepared.commit.clone()
        } else {
            publication
                .scope(editor::publish_staged(
                    self.path(),
                    prepared,
                    staged.as_deref(),
                ))
                .await?
        };
        editor::release_staged(prepared);
        let bypass_followup = record_secret_bypasses(pool, repo_id, actor, bypasses).await;
        let followup = self
            .after_ref_change(
                pool,
                repo_id,
                actor,
                &[(
                    prepared.reference.clone(),
                    prepared
                        .expected
                        .clone()
                        .unwrap_or_else(|| editor::ZERO.to_owned()),
                    commit.sha.clone(),
                )],
            )
            .await;
        record_branch_update(
            pool,
            repo_id,
            actor,
            (self.path(), self.node_id),
            &prepared.branch,
            prepared.expected.as_deref(),
            &prepared.commit.sha,
        )
        .await;
        followup?;
        bypass_followup?;
        Ok(commit)
    }

    /// Follow-ups run on the owning store, including when the caller reached it over RPC.
    async fn after_ref_change(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        moves: &[crate::backend::webhooks::ref_changes::RefMove],
    ) -> Result<(), RepoError> {
        // In-flight contributor proofs may have observed the pre-publication head after
        // the preparation-time version bump. Invalidate them before other follow-ups.
        let contributors = db::repositories::records::advance_contribution_version(pool, repo_id)
            .await
            .map_err(|_| RepoError::applied_followup_failed());
        let activity = crate::backend::repositories::activity::record_moves(
            pool,
            repo_id,
            actor,
            self.path(),
            moves,
        )
        .await
        .map_err(|error| {
            tracing::error!(repo_id,%error,"could not record accepted ref activity");
            RepoError::applied_followup_failed()
        });
        let metadata = self.synchronize_default_branch(pool, repo_id).await;
        refresh_storage_size(pool.clone(), repo_id, self.clone());
        let repository = db::repositories::records::get_repository_by_id(pool, repo_id)
            .await
            .map_err(|_| RepoError::applied_followup_failed())?
            .ok_or_else(RepoError::applied_followup_failed)?;
        crate::backend::repositories::dependencies::observe_refs(pool, &repository, moves).await;
        // Evaluate-only reporting must neither hide accepted native writes nor skip other
        // follow-ups when its storage is unavailable.
        let suites: anyhow::Result<()> = async {
            let user = db::users::accounts::get_user_by_id(pool, actor)
                .await?
                .ok_or_else(|| anyhow::anyhow!("rule suite actor is unavailable"))?;
            let updates: Vec<_> = moves
                .iter()
                .map(|(reference, old, new)| {
                    crate::backend::repositories::git_sessions::validation::RefUpdate {
                        reference: reference.clone(),
                        old: old.clone(),
                        new: new.clone(),
                    }
                })
                .collect();
            crate::backend::repositories::rule_suites::record_push(
                pool,
                &repository,
                &user,
                &updates,
                &History::Stored {
                    repository: self.path(),
                },
                None,
            )
            .await
        }
        .await;
        if let Err(error) = suites {
            tracing::warn!(repo_id, %error, "could not record native rule suites");
        }
        let accepted = async {
            for (reference, _, new) in moves.iter().filter(|(_, _, new)| new != editor::ZERO) {
                // Web and API saves get the same incremental secret scan that a push gets.
                db::secret_scanning::enqueue(pool, repo_id, new, actor)
                    .await
                    .map_err(|_| RepoError::applied_followup_failed())?;
                match self.commit(new).await {
                    Ok(commit) => db::ci::record_ci_accepted_push(
                        pool,
                        repo_id,
                        actor,
                        reference,
                        &commit.sha,
                    )
                    .await
                    .map_err(|_| RepoError::applied_followup_failed())?,
                    // Tags may point at trees or blobs; those never produce a CI run.
                    Err(_) if reference.starts_with("refs/tags/") => {}
                    Err(_) => return Err(RepoError::applied_followup_failed()),
                }
            }
            Ok(())
        }
        .await;
        crate::backend::webhooks::ref_changes::emit_ref_moves(
            pool,
            &repository,
            actor,
            self.path(),
            moves,
        )
        .await;
        git::maintenance::after_push(repo_id, self.path().to_owned());
        accepted.and(metadata).and(activity).and(contributors)
    }

    /// Git has already published the branch. A metadata failure must not imply rollback.
    async fn synchronize_default_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
    ) -> Result<(), RepoError> {
        let result: anyhow::Result<()> = async {
            if let Some(branch) = git::repository::default_branch(self.path()).await? {
                // An existing HEAD already points at this branch; avoid a redundant HEAD.lock.
                let target = format!("refs/heads/{branch}");
                let path = self.path().to_owned();
                let current = self
                    .blocking(move |_| {
                        let repo = git2::Repository::open_bare(&path)?;
                        let target = repo
                            .find_reference("HEAD")?
                            .symbolic_target()
                            .map(str::to_owned);
                        Ok(target)
                    })
                    .await?;
                if current.as_deref() != Some(&target) {
                    self.set_head_target(&target).await?;
                }
                db::repositories::records::update_repo_default_branch(pool, repo_id, &branch)
                    .await?;
            }
            Ok(())
        }
        .await;
        result.map_err(|error| {
            tracing::error!(repo_id, %error, "could not synchronize the default branch after publication");
            RepoError::applied_followup_failed()
        })
    }

    pub(super) async fn update_pull_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        source: (i64, &LocalStore),
        request: &super::PullBranchUpdate,
    ) -> Result<String, RepoError> {
        self.admitted_path(false).await?;
        source.1.admitted_path(true).await?;
        live_read(pool, source.0, actor, source.1.path(), source.1.node_id).await?;
        live_pull_head_write(
            pool,
            (repo_id, source.0, actor),
            request,
            self,
            None,
            &History::SingleCommit,
        )
        .await?;
        let committer = git::Person {
            name: &request.name,
            email: &request.email,
            date: None,
        };
        let prepared = git::repository::prepare_pull_branch_update(
            self.path(),
            source.1.path(),
            &request.head,
            &request.base,
            request.rebase,
            &committer,
        )
        .await
        .map_err(RepoError::from)?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        live_read(pool, source.0, actor, source.1.path(), source.1.node_id).await?;
        live_pull_head_write(
            pool,
            (repo_id, source.0, actor),
            request,
            self,
            None,
            &History::SingleCommit,
        )
        .await?;
        let current = git::repository::current_branch_head(self.path(), &request.branch)
            .await
            .map_err(RepoError::from)?;
        if current.as_deref() == Some(prepared.head()) {
            return Ok(prepared.head().to_owned());
        }
        if current.as_deref() != Some(request.head.as_str()) {
            return Err(RepoError::from(anyhow::Error::new(
                git::repository::PullHeadConflict,
            )));
        }
        let write = BranchWrite {
            branch: &request.branch,
            old: Some(&request.head),
            new: Some(prepared.head()),
            via: WriteVia::Direct,
        };
        live_pull_head_write(
            pool,
            (repo_id, source.0, actor),
            request,
            self,
            Some(&write),
            &History::Stored {
                repository: prepared.path(),
            },
        )
        .await?;
        // This PR-scoped consent is deliberately not general repository write access.
        let user = db::users::accounts::get_user_by_id(pool, actor)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            .ok_or_else(RepoError::write_denied)?;
        let _change = Arc::new(self.begin_publisher_change(pool, repo_id).await?);
        let updates = vec![git::PublicationUpdate {
            reference: git::branch_ref(&request.branch)
                .ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?,
            old: Some(request.head.clone()),
            new: Some(prepared.head().to_owned()),
        }];
        self.prepare_publications(
            pool,
            repo_id,
            git::PublicationActor::User(actor),
            &user.username,
            &updates,
            &publication,
            &_change,
            async {
                live_pull_head_write(
                    pool,
                    (repo_id, source.0, actor),
                    request,
                    self,
                    Some(&write),
                    &History::Stored {
                        repository: prepared.path(),
                    },
                )
                .await?;
                live_read(pool, source.0, actor, source.1.path(), source.1.node_id).await
            },
        )
        .await?;
        // Recheck the exact pull grant and branch policy after preparing publication evidence.
        live_pull_head_write(
            pool,
            (repo_id, source.0, actor),
            request,
            self,
            Some(&write),
            &History::Stored {
                repository: prepared.path(),
            },
        )
        .await?;
        live_read(pool, source.0, actor, source.1.path(), source.1.node_id).await?;
        self.check_publication_grant(
            pool,
            repo_id,
            actor,
            request.token,
            vec![(request.head.clone(), prepared.head().to_owned())],
            Some(prepared.path().to_owned()),
        )
        .await?;
        self.import_prepared_objects(
            pool,
            repo_id,
            prepared.path(),
            &[prepared.head().to_owned()],
        )
        .await?;
        let changes = [oid_change(
            &format!("refs/heads/{}", request.branch),
            Some(&request.head),
            Some(prepared.head()),
        )];
        if !publication
            .scope(self.publish_quorum_refs(pool, repo_id, &changes, async {
                live_pull_head_write(
                    pool,
                    (repo_id, source.0, actor),
                    request,
                    self,
                    Some(&write),
                    &History::Stored {
                        repository: prepared.path(),
                    },
                )
                .await?;
                live_read(pool, source.0, actor, source.1.path(), source.1.node_id).await?;
                self.check_publication_grant(
                    pool,
                    repo_id,
                    actor,
                    request.token,
                    vec![(request.head.clone(), prepared.head().to_owned())],
                    Some(prepared.path().to_owned()),
                )
                .await
            }))
            .await?
        {
            publication
                .scope(git::repository::publish_pull_branch_update(
                    self.path(),
                    &request.branch,
                    &request.head,
                    &prepared,
                ))
                .await
                .map_err(RepoError::from)?;
        }
        let followup = self
            .after_ref_change(
                pool,
                repo_id,
                actor,
                &[(
                    format!("refs/heads/{}", request.branch),
                    request.head.clone(),
                    prepared.head().to_owned(),
                )],
            )
            .await;
        record_branch_update(
            pool,
            repo_id,
            actor,
            (self.path(), self.node_id),
            &request.branch,
            Some(&request.head),
            prepared.head(),
        )
        .await;
        followup?;
        Ok(prepared.head().to_owned())
    }

    pub(super) async fn prepare_revert(
        &self,
        request: &super::RevertRequest,
    ) -> Result<String, RepoError> {
        self.admitted_path(true).await?;
        let path = self.path();
        git::prepare_revert(
            path,
            (&request.current, &request.merged, &request.before),
            (&request.message, &request.name, &request.email),
        )
        .await
        .map_err(RepoError::from)
    }

    pub(super) async fn prepare_merge(&self, request: &MergeRequest) -> Result<String, RepoError> {
        self.admitted_path(true).await?;
        let path = self.path();
        let committer = git::Person {
            name: &request.name,
            email: &request.email,
            date: None,
        };
        let author = git::Person {
            name: request.author_name.as_deref().unwrap_or(&request.name),
            email: request.author_email.as_deref().unwrap_or(&request.email),
            date: None,
        };
        let merged = match request.method {
            MergeMethod::Merge => {
                git::prepare_merge(
                    path,
                    &request.head,
                    &request.base,
                    &request.message,
                    &author,
                    &committer,
                )
                .await
            }
            MergeMethod::Squash => {
                git::prepare_squash(
                    path,
                    &request.head,
                    &request.base,
                    &request.message,
                    request.squash_body.as_deref(),
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
        via: WriteVia,
        token: Option<i64>,
        credential: Option<&db::credentials::Actor>,
        policy: Option<crate::backend::db::pull_requests::merge_policy::Snapshot>,
    ) -> Result<(), RepoError> {
        self.publish_merge_authorized(
            pool,
            repo_id,
            actor,
            heads,
            base_branch,
            base,
            result,
            method,
            via,
            token,
            None,
            credential,
            policy,
        )
        .await
    }
    pub(super) async fn publish_async_merge(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        request: &AsyncMergePublication,
    ) -> Result<(), RepoError> {
        let heads: Vec<_> = request
            .heads
            .iter()
            .map(|(name, sha)| (name.as_str(), sha.as_str()))
            .collect();
        self.publish_merge_authorized(
            pool,
            repo_id,
            actor,
            &heads,
            &request.base_branch,
            &request.base,
            &request.result,
            request.method,
            WriteVia::Merge,
            Some(request.token_id),
            Some(request),
            None,
            request.policy.clone(),
        )
        .await
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Publication holds explicit authority, branch CAS inputs, and the optional durable request."
    )]
    async fn publish_merge_authorized(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        heads: &[(&str, &str)],
        base_branch: &str,
        base: &str,
        result: &str,
        method: MergeMethod,
        via: WriteVia,
        token: Option<i64>,
        authority: Option<&AsyncMergePublication>,
        credential: Option<&db::credentials::Actor>,
        policy: Option<crate::backend::db::pull_requests::merge_policy::Snapshot>,
    ) -> Result<(), RepoError> {
        self.admitted_path(false).await?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let write = BranchWrite {
            branch: base_branch,
            old: Some(base),
            new: Some(result),
            via,
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
        if via == WriteVia::Merge && !allowed_merge_methods(&repository).contains(&method) {
            return Err(RepoError::merge_method_not_allowed());
        }
        let _change = self
            .begin_branch_change(pool, repo_id, actor, &[&write], &publication, None)
            .await?;
        let bypasses =
            crate::backend::repositories::secret_scanning::push_protection::refuse_blocked_secrets(
                pool,
                &repository,
                actor,
                self.path(),
                None,
                &[result.to_owned()],
            )
            .await
            .map_err(RepoError::from)?;
        let authorize = || async {
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
            if via == WriteVia::Merge && !allowed_merge_methods(&repository).contains(&method) {
                return Err(RepoError::merge_method_not_allowed());
            }
            if let Some(authority) = authority {
                db::users::tokens::token_user(pool, authority.token_id, true)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                    .filter(|u| u.id == actor)
                    .ok_or_else(RepoError::write_denied)?;
                let request = db::pull_requests::async_merge::get(
                    pool,
                    repo_id,
                    authority.pull_request_id,
                    &authority.uuid,
                )
                .await
                .map_err(|_| RepoError::authority_unavailable())?
                .filter(|r| r.status == "pending" && r.token_id == Some(authority.token_id))
                .ok_or_else(RepoError::write_denied)?;
                let pull = db::pull_requests::records::get_pull_request_by_id(
                    pool,
                    authority.pull_request_id,
                )
                .await
                .map_err(|_| RepoError::authority_unavailable())?
                .filter(|p| p.repo_id == repo_id && p.state == "open" && !p.merged && !p.is_draft)
                .ok_or_else(RepoError::write_denied)?;
                if request.request["expected_head"].as_str() != Some(pull.head_sha.as_str())
                    || request.request["expected_base"].as_str() != Some(pull.base_sha.as_str())
                    || request.request["expected_base_branch"].as_str()
                        != Some(pull.base_branch.as_str())
                {
                    return Err(RepoError::write_denied());
                }
            }
            self.check_publication_grant(
                pool,
                repo_id,
                actor,
                token,
                vec![(base.to_owned(), result.to_owned())],
                None,
            )
            .await?;
            if let Some(credential) = credential {
                if credential.user_id != actor {
                    return Err(RepoError::write_denied());
                }
                crate::backend::repositories::access::check_publication_actor(
                    pool,
                    credential,
                    &repository,
                )
                .await?;
            }
            // The policy the merge was judged under must still hold as the last step before the ref
            // moves. No transaction is held across the Git process.
            if let Some(expected) = &policy {
                db::pull_requests::merge_policy::check(pool, repo_id, expected)
                    .await
                    .map_err(|error| {
                        if error.is::<db::pull_requests::merge_policy::Changed>() {
                            RepoError::new(ErrorKind::Conflict, error.to_string())
                        } else {
                            RepoError::authority_unavailable()
                        }
                    })?;
            }
            self.verify_merge_heads(heads, Some(base_branch)).await?;
            Ok(())
        };
        authorize().await?;
        let changes = [oid_change(
            &format!("refs/heads/{base_branch}"),
            Some(base),
            Some(result),
        )];
        if !publication
            .scope(self.publish_quorum_refs(pool, repo_id, &changes, authorize()))
            .await?
        {
            publication
                .scope(git::publish_merge(
                    self.path(),
                    heads,
                    base_branch,
                    base,
                    result,
                ))
                .await
                .map_err(RepoError::from)?;
        }
        let followup = self
            .after_ref_change(
                pool,
                repo_id,
                actor,
                &[(
                    format!("refs/heads/{base_branch}"),
                    base.to_owned(),
                    result.to_owned(),
                )],
            )
            .await;
        record_branch_update(
            pool,
            repo_id,
            actor,
            (self.path(), self.node_id),
            base_branch,
            Some(base),
            result,
        )
        .await;
        let bypass_followup = record_secret_bypasses(pool, repo_id, actor, bypasses).await;
        followup?;
        bypass_followup
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
        git::replay_commits(
            &self.admitted_path(true).await?,
            from,
            head,
            onto,
            &committer,
        )
        .await
        .map(|replayed| replayed.tip)
        .map_err(RepoError::from)
    }

    /// Moves each branch in `moves` after checking, under the repository lock, the actor's write
    /// access, each branch's protection rules and the stack's versions. A restack force-updates a
    /// head branch, which the rules that block force pushes refuse.
    #[expect(
        clippy::too_many_arguments,
        reason = "Restack publication carries explicit authority, stack and CAS inputs."
    )]
    pub(super) async fn publish_restack(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        stack_id: i64,
        versions: &[(i64, i64)],
        base_branch: &str,
        base: &str,
        moves: &[RestackMove],
    ) -> Result<(), RepoError> {
        self.admitted_path(false).await?;
        let actor = authority.user_id;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
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
        for restack in moves.iter().filter(|restack| restack.old != restack.new) {
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
        // Retargeting, closing, unstacking and joining change only the database, so the stack is
        // compared here, after the lock wait and the checks, right before the refs move.
        let current = db::pull_requests::records::list_stack_versions(pool, repo_id, stack_id)
            .await
            .map_err(|_| unavailable("Repository storage is unavailable."))?;
        if current != versions {
            return Err(RepoError::stack_changed());
        }
        // Refuse stale user refs before preparing publication-intent refs. The publication
        // lock keeps these checks stable until the final atomic ref transaction.
        if git::repository::current_branch_head(self.path(), base_branch)
            .await
            .map_err(RepoError::from)?
            .as_deref()
            != Some(base)
        {
            return Err(RepoError::new(ErrorKind::Conflict, "Restack base changed"));
        }
        for restack in moves {
            if git::repository::current_branch_head(self.path(), &restack.branch)
                .await
                .map_err(RepoError::from)?
                .as_deref()
                != Some(restack.old.as_str())
            {
                return Err(RepoError::new(ErrorKind::Conflict, "Restack head changed"));
            }
        }
        let writes = moves
            .iter()
            .filter(|restack| restack.old != restack.new)
            .map(|restack| BranchWrite {
                branch: &restack.branch,
                old: Some(&restack.old),
                new: Some(&restack.new),
                via: WriteVia::Direct,
            })
            .collect::<Vec<_>>();
        let references = writes.iter().collect::<Vec<_>>();
        let _change = self
            .begin_branch_change(pool, repo_id, actor, &references, &publication, None)
            .await?;
        let repository = access::current_repository(pool, repo_id)
            .await
            .map_err(|_| RepoError::write_denied())?;
        access::check_publication_actor(pool, &authority, &repository)
            .await
            .map_err(|_| RepoError::write_denied())?;
        let changes = moves
            .iter()
            .map(|restack| {
                oid_change(
                    &format!("refs/heads/{}", restack.branch),
                    Some(&restack.old),
                    Some(&restack.new),
                )
            })
            .collect::<Vec<_>>();
        if !publication
            .scope(self.publish_quorum_refs(pool, repo_id, &changes, async {
                for write in &writes {
                    live_write(
                        pool,
                        repo_id,
                        actor,
                        Some(write),
                        &History::Stored {
                            repository: self.path(),
                        },
                        self.path(),
                        self.node_id,
                    )
                    .await?;
                }
                let repository = access::current_repository(pool, repo_id)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?;
                access::check_publication_actor(pool, &authority, &repository).await?;
                let current =
                    db::pull_requests::records::list_stack_versions(pool, repo_id, stack_id)
                        .await
                        .map_err(|_| RepoError::authority_unavailable())?;
                if current != versions {
                    return Err(RepoError::stack_changed());
                }
                self.verify_merge_heads(&[(base_branch, base)], None)
                    .await?;
                for restack in moves {
                    self.verify_merge_heads(&[(&restack.branch, &restack.old)], None)
                        .await?;
                }
                Ok(())
            }))
            .await?
        {
            publication
                .scope(git::publish_restack(
                    self.path(),
                    base_branch,
                    base,
                    &ref_moves,
                ))
                .await
                .map_err(RepoError::from)?;
        }
        let updates = moves
            .iter()
            .filter(|restack| restack.old != restack.new)
            .map(|restack| {
                (
                    format!("refs/heads/{}", restack.branch),
                    restack.old.clone(),
                    restack.new.clone(),
                )
            })
            .collect::<Vec<_>>();
        let followup = self.after_ref_change(pool, repo_id, actor, &updates).await;
        for restack in moves.iter().filter(|restack| restack.old != restack.new) {
            record_branch_update(
                pool,
                repo_id,
                actor,
                (self.path(), self.node_id),
                &restack.branch,
                Some(restack.old.as_str()),
                &restack.new,
            )
            .await;
        }
        followup
    }

    // ----- Git sessions -----

    pub(super) async fn advertise_refs(&self, service: GitService) -> Result<Vec<u8>, RepoError> {
        let _permit = git::repository::process::session_permit()
            .await
            .map_err(RepoError::from)?;
        git::advertise_service_refs(
            &self
                .admitted_path(service == GitService::UploadPack)
                .await?,
            service.name(),
        )
        .await
        .map_err(RepoError::from)
    }

    /// Runs one `git` command against this repository, with the usual scrubbed
    /// environment and limits.
    async fn git(&self, args: &[&str]) -> Result<(std::process::ExitStatus, Vec<u8>), RepoError> {
        let mut command = process::git_command().map_err(RepoError::from)?;
        command
            .arg("--git-dir")
            .arg(self.admitted_path(true).await?)
            .args(args);
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
        Ok(Some(
            target.strip_suffix('\n').unwrap_or(&target).to_owned(),
        ))
    }

    /// The refs and HEAD that a replica copy takes from this primary, read as one listing.
    /// Refused while a ref change is counted here: the copy read the version before it asked,
    /// and a change counted before that read may not be made yet.
    pub(super) async fn replica_listing(
        &self,
    ) -> Result<(HashMap<String, String>, Option<String>), RepoError> {
        let key = counting_key(self.path()).await;
        let busy = ref_change_counts().contains_key(&key);
        if busy {
            return Err(RepoError::replica_busy());
        }
        // A cancelled caller's Git child may still own the lease after its in-process
        // counter disappears. Never advertise a replica snapshot before that child exits.
        let _publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        Ok((self.mirror_refs().await?, self.head_target().await?))
    }

    pub(super) async fn existing_commits(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashSet<String>, RepoError> {
        codeyard_git::mirror::existing_commits(&self.admitted_path(true).await?, ids)
            .await
            .map_err(RepoError::from)
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
            anyhow::ensure!(
                codeyard_git::is_full_commit_id(target),
                "invalid replica ref target"
            );
            revs.push_str(target);
            revs.push('\n');
        }
        let present = self.existing_commits(have).await?;
        for tip in have.iter().filter(|tip| present.contains(*tip)) {
            anyhow::ensure!(codeyard_git::is_full_commit_id(tip), "invalid replica tip");
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
    pub(super) async fn index_pack(&self, pack: &Path, strict: bool) -> Result<(), RepoError> {
        let mut input = tokio::fs::File::open(pack)
            .await
            .map_err(|_| unavailable("Git could not read the replica pack"))?;
        let mut command = process::git_command().map_err(RepoError::from)?;
        command
            .arg("--git-dir")
            .arg(self.path())
            .args(["index-pack", "--stdin"]);
        if strict {
            // Imported history must be complete. Strict indexing refuses objects that link to
            // missing ones, which would otherwise be published as refs.
            command.arg("--strict");
        }
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

    /// Makes this copy's refs exactly `refs`. Obsolete namespace blockers are removed
    /// first: Git's files backend cannot lock both `topic` and `topic/sub` in one transaction.
    /// The objects they name must already be stored.
    pub(super) async fn set_refs(&self, refs: &HashMap<String, String>) -> Result<(), RepoError> {
        let mut current = self.mirror_refs().await?;
        let names: std::collections::BTreeSet<&str> = refs.keys().map(String::as_str).collect();
        let blockers: Vec<String> = current
            .keys()
            .filter(|name| {
                if refs.contains_key(*name) {
                    return false;
                }
                let prefix = format!("{name}/");
                names
                    .range(prefix.as_str()..)
                    .next()
                    .is_some_and(|new| new.starts_with(&prefix))
                    || name
                        .match_indices('/')
                        .any(|(at, _)| refs.contains_key(&name[..at]))
            })
            .cloned()
            .collect();
        for name in blockers {
            let old = &current[&name];
            let (status, _) = self.git(&["update-ref", "-d", &name, old]).await?;
            if !status.success() {
                return Err(unavailable("Git could not remove an obsolete replica ref"));
            }
            current.remove(&name);
        }
        let mut script = String::from("start\n");
        for (name, sha) in refs {
            if !name.starts_with("refs/") || !git2::Reference::is_valid_name(name) {
                return Err(RepoError::new(
                    ErrorKind::Invalid,
                    "invalid replica ref name",
                ));
            }
            if !codeyard_git::is_full_commit_id(sha) {
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

    pub(super) async fn set_default_branch(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: db::credentials::Actor,
        name: &str,
    ) -> Result<(), RepoError> {
        self.patch_repository(pool, repo_id, actor, name, Default::default())
            .await
            .map(|_| ())
    }

    /// HEAD is changed before the single metadata transaction, so a Git failure cannot leave
    /// partially applied metadata. A refused transaction restores the exact previous HEAD.
    pub(super) async fn transfer_repository(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        new_owner: &str,
    ) -> Result<db::models::Repository, RepoError> {
        let actor = authority.user_id;
        let (store, pool, new_owner) = (self.clone(), pool.clone(), new_owner.to_owned());
        tokio::spawn(async move {
            let _lock = git::locks::lock(store.path()).await;
            let repository = access::check_admin(&pool, repo_id, actor)
                .await
                .map_err(|error| {
                    if error == access::AccessError::Unavailable {
                        RepoError::authority_unavailable()
                    } else {
                        RepoError::write_denied()
                    }
                })?;
            if !repository_matches_path(&repository, store.path(), store.node_id) {
                return Err(RepoError::write_denied());
            }
            access::credential_user(&pool, &authority, true)
                .await
                .map_err(RepoError::from)?;
            let (target, owner) =
                crate::backend::repositories::lifecycle::transfer_target(&pool, &new_owner)
                    .await
                    .map_err(|error| RepoError::new(ErrorKind::Invalid, error.message))?;
            crate::backend::repositories::releases::immutable::check_namespace(
                &pool,
                &repository,
                &owner,
                &repository.name,
                &store.refs().await?,
            )
            .await?;
            db::repositories::records::transfer_repository_checked(
                &pool,
                authority,
                repo_id,
                target,
                &owner,
                &repository.name,
            )
            .await
            .map_err(|error| {
                use crate::backend::repositories::lifecycle::{storage_error, LifecycleFailure};
                let error = storage_error(error);
                let kind = match error.kind {
                    LifecycleFailure::Denied => return RepoError::write_denied(),
                    LifecycleFailure::Conflict => ErrorKind::Conflict,
                    LifecycleFailure::Invalid => ErrorKind::Invalid,
                    _ => ErrorKind::Unavailable,
                };
                RepoError::new(kind, error.message)
            })
        })
        .await
        .map_err(|_| unavailable("Repository transfer failed"))?
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The store forwards the dependency-security request's explicit fields."
    )]
    pub(super) async fn configure_dependency_security(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        credential: db::users::accounts::OwnedAccountCredential,
        feature: db::repositories::dependencies::Feature,
        enabled: bool,
        private_consent: bool,
    ) -> Result<(), RepoError> {
        let (store, pool) = (self.clone(), pool.clone());
        tokio::spawn(async move {
            let _lock = git::locks::lock(store.path()).await;
            live_write(
                &pool,
                repo_id,
                actor,
                None,
                &History::SingleCommit,
                store.path(),
                store.node_id,
            )
            .await?;
            db::repositories::dependencies::configure(
                &pool,
                (actor, &credential),
                repo_id,
                feature,
                enabled,
                private_consent,
            )
            .await
            .map_err(crate::backend::repositories::dependencies::store_error)
        })
        .await
        .map_err(|_| unavailable("Dependency security configuration failed"))?
    }

    /// The tag has already been created through the normal ref pipeline. Only its
    /// publication-time proof and release SQL are committed under this Git lease.
    pub(super) async fn mutate_release(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        mut mutation: crate::backend::repositories::releases::immutable::Mutation,
        attestation: Option<crate::backend::repositories::releases::integrity::Attestation>,
    ) -> Result<(db::models::Release, bool), RepoError> {
        use crate::backend::repositories::releases::immutable::Mutation;
        mutation.validate()?;
        let (store, pool) = (self.clone(), pool.clone());
        tokio::spawn(async move {
            let (tag, publishing) = match &mut mutation {
                Mutation::Create { tag, fields, .. } => (tag.clone(), !fields.is_draft),
                Mutation::Update { id, changes } => {
                    let current = db::releases::get_release(&pool, repo_id, *id)
                        .await
                        .map_err(|_| unavailable("Release storage unavailable"))?
                        .ok_or_else(|| RepoError::new(ErrorKind::NotFound, "Release not found"))?
                        .release;
                    let seen = *changes.expected_draft.get_or_insert(current.is_draft);
                    let publishing = seen && changes.is_draft == Some(false);
                    (changes.tag.clone().unwrap_or(current.tag_name), publishing)
                }
            };
            // Only a write that reads the tag waits for ref publishers. Editing the text of a
            // published release is a database write and works while Git storage is offline;
            // its transaction checks the revision the form was rendered with, and refuses the
            // edit if the release became a draft meanwhile, so nothing publishes unchecked.
            let reads_tag =
                matches!(&mutation, Mutation::Create { .. }) || publishing || attestation.is_some();
            let _publication = if reads_tag {
                Some(
                    git::locks::publication(store.path())
                        .await
                        .map_err(RepoError::from)?,
                )
            } else {
                None
            };
            let repository = live_write(
                &pool,
                repo_id,
                actor,
                None,
                &History::SingleCommit,
                store.path(),
                store.node_id,
            )
            .await?;
            let mut target = String::new();
            let mut tag_oid = String::new();
            if reads_tag {
                let reference =
                    git::tag_ref(&tag).ok_or_else(|| codeyard_git::error::invalid_ref("tag"))?;
                let draft_target = match &mutation {
                    Mutation::Create {
                        fields,
                        draft_target,
                        ..
                    } if fields.is_draft && attestation.is_none() => draft_target.clone(),
                    _ => None,
                };
                match (store.refs().await?.remove(&reference), draft_target) {
                    (Some(oid), _) => {
                        target = store.commit(&oid).await?.sha;
                        tag_oid = oid;
                    }
                    // A draft whose tag does not exist yet keeps the commit it was saved for.
                    (None, Some(commit)) => target = store.commit(&commit).await?.sha,
                    (None, None) => {
                        return Err(RepoError::new(
                            ErrorKind::Conflict,
                            "The release tag changed; retry publication",
                        ))
                    }
                }
            }
            // Accept only the local identity or active configured forge nodes,
            // independently of the signature's self-declared key.
            let publication = if let Some(attestation) = &attestation {
                let key = &attestation
                    .signatures
                    .first()
                    .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "Missing release signature"))?
                    .keyid;
                let verified = if db::cluster::is_active_node(&pool, key)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                {
                    db::repositories::immutable_releases::Publication::verify(attestation, key)
                        .map_err(|_| {
                            RepoError::new(ErrorKind::Invalid, "Invalid release attestation")
                        })?
                } else if store.node_id.flatten().is_none() {
                    crate::backend::repositories::releases::immutable::verify_local(
                        attestation,
                        store.path(),
                    )
                    .await?
                } else {
                    return Err(RepoError::new(
                        ErrorKind::Invalid,
                        "Untrusted release signer",
                    ));
                };
                let predicate = &verified.statement.predicate;
                if predicate.repository_id != repository.id
                    || predicate.tag != tag
                    || predicate.tag_oid != tag_oid
                    || predicate.commit_sha != target
                {
                    return Err(RepoError::new(
                        ErrorKind::Conflict,
                        "The release tag changed; retry publication",
                    ));
                }
                Some(verified)
            } else {
                None
            };
            let result = match mutation {
                Mutation::Create { tag, fields, .. } => {
                    db::releases::create_release_with_publication(
                        &pool,
                        actor,
                        repo_id,
                        &tag,
                        &target,
                        &fields,
                        publication.as_ref(),
                    )
                    .await
                    .map(|release| {
                        let published = !release.is_draft;
                        (release, published)
                    })
                }
                Mutation::Update { id, changes } => {
                    db::releases::update_release_with_publication(
                        &pool,
                        actor,
                        repo_id,
                        id,
                        &changes,
                        publication.as_ref(),
                    )
                    .await
                }
            };
            result.map_err(|error| {
                use db::releases::ReleaseRefusal;
                let kind = match error.downcast_ref::<ReleaseRefusal>() {
                    Some(ReleaseRefusal::Denied) => return RepoError::write_denied(),
                    Some(ReleaseRefusal::NotFound) => ErrorKind::NotFound,
                    Some(ReleaseRefusal::Conflict) => ErrorKind::Conflict,
                    Some(_) => ErrorKind::Invalid,
                    None => return unavailable("Release storage unavailable"),
                };
                RepoError::new(kind, error.to_string())
            })
        })
        .await
        .map_err(|_| unavailable("Release publication failed"))?
    }

    pub(super) async fn patch_repository(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: db::credentials::Actor,
        name: &str,
        changes: db::repositories::records::RepositoryChanges<String>,
    ) -> Result<db::models::Repository, RepoError> {
        // Publication and compensation finish even when the client disconnects.
        let (store, pool, name) = (self.clone(), pool.clone(), name.to_owned());
        tokio::spawn(async move {
            store
                .patch_repository_owned(&pool, repo_id, actor, &name, &changes)
                .await
        })
        .await
        .map_err(|_| unavailable("Repository update failed"))?
    }

    async fn patch_repository_owned(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        authority: db::credentials::Actor,
        name: &str,
        changes: &db::repositories::records::RepositoryChanges<String>,
    ) -> Result<db::models::Repository, RepoError> {
        let actor = authority.user_id;
        let _lock = git::locks::lock(self.path()).await;
        let previous =
            access::check_admin(pool, repo_id, actor)
                .await
                .map_err(|error| match error {
                    access::AccessError::Unavailable => RepoError::authority_unavailable(),
                    _ => RepoError::write_denied(),
                })?;
        if !repository_matches_path(&previous, self.path(), self.node_id) {
            return Err(RepoError::write_denied());
        }
        if previous.is_archived && changes.archived != Some(false) {
            return Err(RepoError::write_denied());
        }
        let checked_owner = if let Some(destination) = changes.name.as_deref() {
            let owner = db::repositories::records::repository_owner_names(pool, &[repo_id])
                .await
                .map_err(RepoError::from)?
                .remove(&repo_id)
                .ok_or_else(|| unavailable("Repository owner unavailable"))?;
            crate::backend::repositories::releases::immutable::check_namespace(
                pool,
                &previous,
                &owner,
                destination,
                &self.refs().await?,
            )
            .await?;
            Some(owner)
        } else {
            None
        };
        let reference =
            git::branch_ref(name).ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?;
        let (status, _) = self
            .git(&["show-ref", "--verify", "--quiet", &reference])
            .await?;
        if !status.success() {
            return Err(RepoError::new(
                ErrorKind::NotFound,
                "That branch does not exist",
            ));
        }
        let previous_head = self.head_target().await?;
        let previous_oid = if previous_head.is_none() {
            let (status, bytes) = self.git(&["rev-parse", "--verify", "HEAD"]).await?;
            if !status.success() {
                return Err(unavailable("Git could not read HEAD"));
            }
            String::from_utf8(bytes)
                .map_err(|_| unavailable("Git returned an invalid HEAD"))?
                .trim()
                .to_owned()
        } else {
            String::new()
        };
        let _change = begin_ref_change(pool, repo_id, self.path(), self.node_id).await?;
        access::credential_user(pool, &authority, true)
            .await
            .map_err(RepoError::from)?;
        self.set_head_target(&reference).await?;
        let updated = db::repositories::records::patch_repository_and_default(
            pool,
            authority,
            repo_id,
            &changes.borrowed(),
            name,
            checked_owner.as_deref(),
        )
        .await;
        let repository = match updated {
            Ok(repository) => repository,
            Err(error) => {
                // A failed COMMIT acknowledgement may still have committed. Do not roll HEAD
                // back when the database already names the requested default.
                use db::repositories::records::RepositorySettingsRefusal;
                let must_restore = if error.downcast_ref::<RepositorySettingsRefusal>().is_some()
                    || error.is::<codeyard_support::credentials::AccountRestricted>()
                    || previous.default_branch == name
                {
                    true
                } else {
                    match db::repositories::records::get_repository_by_id(pool, repo_id).await {
                        Ok(Some(repository)) => repository.default_branch != name,
                        Ok(None) => true,
                        Err(_) => return Err(unavailable(
                            "Repository update could not be confirmed; reconciliation is required",
                        )),
                    }
                };
                if must_restore {
                    let restored = match previous_head.as_deref() {
                        Some(head) => self.set_head_target(head).await,
                        None => self
                            .git(&["update-ref", "--no-deref", "HEAD", &previous_oid])
                            .await
                            .and_then(|(status, _)| {
                                if status.success() {
                                    Ok(())
                                } else {
                                    Err(unavailable("Git could not restore HEAD"))
                                }
                            }),
                    };
                    if restored.is_err() {
                        return Err(unavailable("Repository update failed and HEAD could not be restored; reconciliation is required"));
                    }
                }
                if error.is::<codeyard_support::credentials::AccountRestricted>() {
                    return Err(RepoError::write_denied());
                }
                return Err(match error.downcast_ref::<RepositorySettingsRefusal>() {
                    Some(
                        RepositorySettingsRefusal::Denied
                        | RepositorySettingsRefusal::VisibilityLocked,
                    ) => RepoError::write_denied(),
                    Some(_) => RepoError::new(ErrorKind::Invalid, error.to_string()),
                    None => unavailable("Repository storage is unavailable"),
                });
            }
        };
        languages::refresh_default_branch(
            pool.clone(),
            repo_id,
            self.path().to_owned(),
            name.to_owned(),
            self.node_id,
        );
        Ok(repository)
    }

    /// Points HEAD at a branch, such as `refs/heads/main`.
    pub(super) async fn set_head_target(&self, target: &str) -> Result<(), RepoError> {
        if !target.starts_with("refs/heads/") || !git2::Reference::is_valid_name(target) {
            return Err(RepoError::new(ErrorKind::Invalid, "invalid HEAD target"));
        }
        if let Some((context, repo)) = &self.context {
            let pool = context.database()?;
            if self.quorum_context(pool, *repo).await?.is_some() {
                let old = self
                    .head_target()
                    .await?
                    .ok_or_else(|| unavailable("Detached HEAD requires recovery"))?;
                let changes = [db::models::RefChange {
                    name: "HEAD".into(),
                    kind: "symbolic".into(),
                    old_target: Some(old),
                    new_target: Some(target.to_owned()),
                }];
                self.publish_quorum_refs(pool, *repo, &changes, async {
                    self.admitted_path(false).await?;
                    Ok(())
                })
                .await?;
                return Ok(());
            }
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
        let path = self
            .admitted_path(session.service == GitService::UploadPack)
            .await?;
        if session.service == GitService::ReceivePack {
            let (pool, ci) =
                receive.ok_or_else(|| anyhow::anyhow!("receive resources required"))?;
            let uuid = super::rpc::repository_uuid(&self.disk_path)?;
            let repo = db::cluster::get_repository_by_uuid(&pool, &uuid.to_string())
                .await?
                .ok_or_else(|| anyhow::anyhow!("repository unavailable"))?;
            crate::backend::cluster::context::check_term_directory(
                &pool,
                &repo,
                self.node_id.flatten(),
                &path,
            )
            .await?;
            let result = git_sessions::receive_session(
                pool.clone(),
                ci,
                git_sessions::ReceiveSession {
                    repo_id: repo.id,
                    path: &path,
                    executable: hook_executable()?,
                    session,
                },
                streams,
            )
            .await;
            return result;
        }
        let mut command = process::git_command()?;
        if session.allow_unreachable {
            command.args(["-c", "uploadpack.allowAnySHA1InWant=true"]);
        }
        command.arg("upload-pack");
        if session.stateless {
            command.arg("--stateless-rpc");
        }
        if let Some(protocol) = git_sessions::validation::protocol(session.git_protocol.as_deref())?
        {
            command.env("GIT_PROTOCOL", protocol);
        }
        command.arg(&path);
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

/// The binary Git runs as the receive hooks. Rust unit tests are not the application, so they
/// get the integrated binary from the environment.
fn hook_executable() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    {
        use anyhow::Context;
        Ok(PathBuf::from(
            std::env::var_os("GITSERV_TEST_OPENSSH_HELPER")
                .context("set GITSERV_TEST_OPENSSH_HELPER to the integrated application")?,
        ))
    }
    #[cfg(not(test))]
    {
        Ok(std::env::current_exe()?)
    }
}
