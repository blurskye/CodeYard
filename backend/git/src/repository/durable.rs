//! The durability barrier for a repository's metadata and object entries.
//!
//! Git with `core.fsync=all` makes the bytes of a ref or object durable, but not the
//! directory entry that names it. Measured on Git 2.54.0 with this server's argv:
//! `init --bare` and `symbolic-ref HEAD` sync nothing, `update-ref` syncs only its
//! lock file, `index-pack` syncs the pack file contents, and `receive-pack` links
//! quarantined objects into `objects/<2hex>` and packs into `objects/pack` without
//! syncing those directories. A crash can therefore keep a ref and lose the objects it names.
//!
//! This module is the one owner of the rule that closes that gap: object-store entries are
//! synced first, then each named metadata file is opened without following a symlink at any
//! component, checked for its type, and every ancestor directory is synced deepest first up
//! to and including the repository root. The loose object store is covered by its fixed set
//! of 256 two-hex-digit names, so the barrier never lists or walks object files.
//!
//! The barrier is bounded twice: collection stops at a configured count and byte budget
//! (including the ancestor directories a deeply nested name expands into), and the blocking
//! work checks the caller's deadline before every entry. It runs on Tokio's blocking pool
//! under its own semaphore and deliberately takes no Git operation permit: a receive
//! acknowledgment runs while the receive child's hooks and control slots are still around, so
//! waiting for a Git process slot there could let the two wait on each other. A cancelled
//! caller leaves the idempotent blocking task to finish, and the semaphore permit is released
//! with it.
//!
//! The same collection budgets are exposed as `RepositorySync::preflight`, which expands the
//! names a publication is about to write into their ancestor directories on the blocking pool
//! under the barrier's own semaphore, without opening the repository. A plan that cannot fit is
//! refused before the refs it names are committed rather than after. It bounds the plan, not
//! the storage: a failed fsync, a short read or a missing entry can still fail the blocking
//! barrier after the refs are written.

use anyhow::{anyhow, bail, ensure, Context, Result};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use super::MAX_MIRROR_REFS;
use super::MAX_MIRROR_REF_BYTES;

/// The bound for a barrier whose caller has no tighter configured deadline.
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(30);
/// The most named files one barrier may sync. A replica write may name both the refs it
/// sets and the refs its copy held before, so two mirror listings are allowed.
pub const MAX_SYNC_FILES: usize = 2 * MAX_MIRROR_REFS;
/// The most bytes of names one barrier may hold. The ancestor directories a deeply nested
/// name expands into are charged against their own budget of the same size.
pub const MAX_SYNC_BYTES: usize = 2 * MAX_MIRROR_REF_BYTES;
/// Git's packed refs live beside the loose ones in the repository root.
const PACKED_REFS: &str = "packed-refs";
/// The longest name one entry may have. Ref admission is stricter; this only bounds a
/// caller that builds names itself.
const MAX_NAME_BYTES: usize = 4 * 1024;
const TIMED_OUT: &str = "Repository sync timed out";
const INVALID: &str = "Invalid repository metadata";

static SYNC_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(sync_limit())));

/// How many barriers run at once: `REPOSITORY_SYNC_LIMIT`, or two per CPU core and at
/// least four. This is independent of Git's process slots on purpose.
fn sync_limit() -> usize {
    configured_sync_limit(std::env::var("REPOSITORY_SYNC_LIMIT").ok().as_deref())
}

/// The configured capacity, or the bounded default. Zero, a non-number, and anything above
/// what a semaphore can hold fall back to the default, so `Semaphore::new` cannot panic on a
/// configured or computed value.
fn configured_sync_limit(value: Option<&str>) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |cores| cores.get());
    let default = cores.saturating_mul(2).clamp(4, Semaphore::MAX_PERMITS);
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| (1..=Semaphore::MAX_PERMITS).contains(limit))
        .unwrap_or(default)
}

/// What a named entry must be. A missing required entry is a failure, because the caller
/// says the record that names it was written. A missing optional entry is the expected
/// result of a deletion.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    RequiredFile,
    OptionalFile,
    /// A ref name Git may have deleted, or replaced with the directory that now holds its
    /// nested names. Both outcomes are recorded state; either directory entry needs syncing.
    OptionalRef,
    RequiredDir,
}

/// What a directory entry must be. A ref rename can leave a ref file where the old nested
/// namespace used to be, so that case is recorded state rather than a failure.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DirKind {
    RefAncestor,
    Optional,
    Required,
}

/// One bounded durability barrier over the repository at a path.
#[derive(Default)]
pub struct RepositorySync {
    entries: Vec<(Entry, String)>,
    bytes: usize,
    overflow: bool,
    objects: bool,
    inventory: bool,
    parent: bool,
}

impl RepositorySync {
    pub fn new() -> Self {
        Self::default()
    }

    /// A metadata file that must exist, such as `HEAD`.
    pub fn required_file(self, name: impl Into<String>) -> Self {
        self.push(Entry::RequiredFile, name)
    }

    /// A metadata file whose absence is a normal outcome, such as `shallow`.
    pub fn optional_file(self, name: impl Into<String>) -> Self {
        self.push(Entry::OptionalFile, name)
    }

    /// A ref name. Its absence is a deletion or a rename, and a directory in its place is
    /// the namespace that replaced it, so either is synced rather than refused.
    pub fn optional_ref(self, name: impl Into<String>) -> Self {
        self.push(Entry::OptionalRef, name)
    }

    /// A directory that must exist, such as `refs/tags`. A rename replaces the
    /// directory entry that names it, so a missing one is a failure rather than a skip.
    pub fn required_dir(self, name: impl Into<String>) -> Self {
        self.push(Entry::RequiredDir, name)
    }

    /// The barrier for published refs: each name, `packed-refs`, every ancestor directory
    /// deepest first, the object-store entries and the root. A ref must never become durable
    /// before the objects it names, and a push can create both in the same operation.
    pub fn refs(names: impl IntoIterator<Item = String>) -> Self {
        // A branch rename of the checked-out branch rewrites HEAD in the same operation, and
        // its file is synced by Git never. An absent HEAD is left to the caller's own check.
        let mut sync = Self::new()
            .optional_ref("HEAD")
            .optional_ref(PACKED_REFS)
            .objects();
        for name in names {
            sync = sync.optional_ref(name);
        }
        sync
    }

    /// Every current ref and surviving ref directory, including empty namespaces left by
    /// deletions. This repairs the inventory when a receive session carried no ref updates.
    pub fn inventory(mut self) -> Self {
        self.inventory = true;
        self
    }

    /// The fixed object-store entries: `objects`, `objects/info`, `objects/pack` and
    /// the 256 two-hex-digit loose object directories that exist. No object file is listed
    /// or walked.
    pub fn objects(mut self) -> Self {
        self.objects = true;
        self
    }

    /// The directory that holds the repository. A create, install or rename replaces that
    /// entry, so it is synced after the repository root. A `HEAD` write alone does
    /// not need it, but the callers that pass this do.
    pub fn parent(mut self) -> Self {
        self.parent = true;
        self
    }

    /// Syncs the barrier. The blocking work keeps its own copy and completes even if the
    /// caller is cancelled, because it is idempotent and the caller's failure must not
    /// leave the entry unsynced.
    pub async fn sync(self, path: &Path, deadline: Instant) -> Result<()> {
        self.validate()?;
        let path = path.to_owned();
        run_bounded(deadline, move || sync_now(&path, &self, deadline)).await
    }

    /// The same barrier for a caller already running on a bounded blocking worker, such as a
    /// publication prepared inside `spawn_blocking`. It cannot queue on the async sync
    /// semaphore; that worker's own admission bounds it. Every entry still checks the deadline.
    pub fn sync_blocking(self, path: &Path, deadline: Instant) -> Result<()> {
        self.validate()?;
        sync_now(path, &self, deadline)
    }

    /// Checks that this barrier's named entries fit the same count and byte budgets the
    /// blocking barrier enforces, without opening or changing the repository.
    ///
    /// Each reference is expanded into the ancestor directories `sync` collects, charged once
    /// per unique directory for the whole plan. A ref or required directory also charges its
    /// final component, because that component is the directory a namespace inversion can
    /// leave in place of the renamed ref. The planned set is therefore a superset of the
    /// directories the barrier finds, so a plan that passes here cannot fail its budget after
    /// the caller's refs are committed. The superset can also refuse a plan the barrier would
    /// have fit, by at most one final component per ref or required directory.
    ///
    /// An `inventory` discovers directories that only exist on disk, so it has no plan and is
    /// refused. The barrier's own `HEAD` and `packed-refs` entries are part of the plan, so
    /// their name bytes and possible directory entries are charged here as well. The object
    /// entries are a fixed set outside this budget, and the check covers the repository path it
    /// is given, not another node's copy. This plans a publication batch; a mirror-sized listing
    /// that names every ref is still bounded by the barrier itself, not by this check.
    ///
    /// The plan runs on the blocking pool under the barrier's own semaphore, so a hook task
    /// never expands up to a budget's worth of path names on an async worker and no Git
    /// process permit is taken. A cancelled caller leaves the read-only plan to finish, and
    /// the permit is released with it.
    pub async fn preflight(self, path: &Path, deadline: Instant) -> Result<()> {
        self.check_plan()?;
        let path = path.to_owned();
        run_bounded(deadline, move || preflight_now(&path, &self, deadline)).await
    }

    /// The same plan for a caller already running on a bounded blocking worker. It cannot
    /// queue on the async sync semaphore; that worker's own admission bounds it.
    pub fn preflight_blocking(&self, path: &Path, deadline: Instant) -> Result<()> {
        self.check_plan()?;
        preflight_now(path, self, deadline)
    }

    fn check_plan(&self) -> Result<()> {
        ensure!(!self.inventory, "A repository inventory cannot be planned");
        self.validate()
    }

    fn push(mut self, entry: Entry, name: impl Into<String>) -> Self {
        let name = name.into();
        if self.entries.len() >= MAX_SYNC_FILES
            || self.bytes.saturating_add(name.len()) > MAX_SYNC_BYTES
        {
            // The caller asked for more than one barrier may hold. Stop collecting now;
            // the barrier fails before it does any I/O.
            self.overflow = true;
            return self;
        }
        self.bytes += name.len();
        self.entries.push((entry, name));
        self
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.overflow, "Too many repository metadata entries");
        for (_, name) in &self.entries {
            validate_name(name)?;
        }
        Ok(())
    }
}

/// One ref publication, transfer or restore barrier over the named refs.
pub async fn sync_refs(path: &Path, names: Vec<String>, deadline: Instant) -> Result<()> {
    RepositorySync::refs(names).sync(path, deadline).await
}

/// Syncs `path` and each ancestor up to and including `root`, deepest first. A recovery install
/// renames a staged repository into a nested location, so every directory entry on the way to
/// the storage root is new.
pub async fn sync_ancestors(path: &Path, root: &Path, deadline: Instant) -> Result<()> {
    ensure!(path.starts_with(root), "Invalid repository sync path");
    let path = path.to_owned();
    let root = root.to_owned();
    run_bounded(deadline, move || {
        let mut directory = Some(path);
        while let Some(current) = directory {
            sync_dir(&current, DirKind::Required, deadline)?;
            if current == root {
                break;
            }
            directory = current.parent().map(Path::to_owned);
        }
        Ok(())
    })
    .await
}

/// Runs one blocking barrier or plan under the sync semaphore. Waiting for the permit and
/// waiting for the result are both bounded by the caller's deadline. A cancelled caller leaves
/// the blocking task (and its permit) to finish, so the entry is still synced.
async fn run_bounded<T: Send + 'static>(
    deadline: Instant,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let wait = deadline.saturating_duration_since(Instant::now());
    ensure!(!wait.is_zero(), TIMED_OUT);
    let permit = tokio::time::timeout(wait, Arc::clone(&*SYNC_SLOTS).acquire_owned())
        .await
        .map_err(|_| anyhow!(TIMED_OUT))?
        .map_err(|_| anyhow!("Repository sync limit closed"))?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    });
    let wait = deadline.saturating_duration_since(Instant::now());
    ensure!(!wait.is_zero(), TIMED_OUT);
    match tokio::time::timeout(wait, task).await {
        Ok(joined) => joined.context("Repository sync task failed")?,
        Err(_) => bail!(TIMED_OUT),
    }
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_NAME_BYTES
            && !name.as_bytes().contains(&0)
            && Path::new(name)
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "Invalid repository metadata name"
    );
    Ok(())
}

fn preflight_now(path: &Path, request: &RepositorySync, deadline: Instant) -> Result<()> {
    // Start from the same entry `sync_now` inserts before it collects anything, so the
    // file count and the byte total are compared against the same starting map.
    let mut directories = BTreeMap::new();
    directories.insert(path.to_owned(), DirKind::Required);
    let mut bytes = 0usize;
    for (entry, name) in &request.entries {
        ensure!(Instant::now() < deadline, TIMED_OUT);
        let target = path.join(name.as_str());
        let kind = match entry {
            Entry::RequiredDir => Some(DirKind::Required),
            Entry::OptionalRef => Some(DirKind::Optional),
            Entry::RequiredFile | Entry::OptionalFile => None,
        };
        if let Some(kind) = kind {
            insert_directory(&mut directories, &mut bytes, target.clone(), kind)?;
        }
        add_ancestors(&target, path, &mut directories, &mut bytes, deadline)?;
    }
    Ok(())
}

fn sync_now(path: &Path, request: &RepositorySync, deadline: Instant) -> Result<()> {
    // The object entries go first. A ref made durable before the entry that names its object
    // can survive alone. A receive also runs this object-only barrier before Git commits its
    // refs (the reference-transaction seam); this post-write order narrows the window but a
    // crash between a ref write and this barrier still needs repair.
    if request.objects {
        sync_objects(path, deadline)?;
    }
    let mut directories = BTreeMap::new();
    directories.insert(path.to_owned(), DirKind::Required);
    let mut directory_bytes = 0usize;
    for (entry, name) in &request.entries {
        sync_entry(
            path,
            *entry,
            name,
            &mut directories,
            &mut directory_bytes,
            deadline,
        )?;
    }
    if request.inventory {
        // The last ref in a namespace may have been deleted before a failed barrier.
        // A live-ref listing cannot find that deletion's now-empty parent directories.
        collect_ref_directories(path, &mut directories, &mut directory_bytes, deadline)?;
        for name in super::list_mirror_refs(path)?.into_keys() {
            sync_entry(
                path,
                Entry::OptionalRef,
                &name,
                &mut directories,
                &mut directory_bytes,
                deadline,
            )?;
        }
    }
    // A path sorts before every path it contains, so the reverse order syncs each child
    // directory before its parent, all the way to the repository root.
    for (directory, kind) in directories.iter().rev() {
        sync_dir(directory, *kind, deadline)?;
    }
    if request.parent {
        let parent = parent_directory(path);
        if let Some(parent) = parent {
            sync_dir(&parent, DirKind::Required, deadline)?;
        }
    }
    Ok(())
}

fn sync_entry(
    root: &Path,
    entry: Entry,
    name: &str,
    directories: &mut BTreeMap<PathBuf, DirKind>,
    bytes: &mut usize,
    deadline: Instant,
) -> Result<()> {
    ensure!(Instant::now() < deadline, TIMED_OUT);
    let required = matches!(entry, Entry::RequiredFile | Entry::RequiredDir);
    // Every component below the repository root is opened without following a symlink before
    // the entry itself is touched, so a symlinked `refs`/`objects` cannot redirect the
    // barrier. A component that is a ref file means a rename replaced the old namespace: the
    // deeper name cannot exist, so an optional entry is absent and a required one fails.
    if !ancestors_are_directories(root, name, deadline)? {
        ensure!(!required, "Missing repository metadata");
        if matches!(entry, Entry::OptionalRef) {
            // A deleted nested ref can take its directory with it. The existing ancestors
            // record that removal, so they are still synced; missing ones are skipped.
            add_ancestors(&root.join(name), root, directories, bytes, deadline)?;
        }
        return Ok(());
    }
    let target = root.join(name);
    match entry {
        Entry::RequiredFile => {
            sync_file(&target, true, deadline)?;
            add_ancestors(&target, root, directories, bytes, deadline)
        }
        Entry::OptionalFile => {
            sync_file(&target, false, deadline)?;
            add_ancestors(&target, root, directories, bytes, deadline)
        }
        Entry::OptionalRef => {
            match open_no_follow(&target, deadline)? {
                None => {}
                Some((file, metadata)) => {
                    if metadata.is_dir() {
                        insert_directory(directories, bytes, target.clone(), DirKind::Optional)?;
                    } else if metadata.is_file() {
                        file.sync_all()?;
                    } else {
                        bail!(INVALID);
                    }
                }
            }
            add_ancestors(&target, root, directories, bytes, deadline)
        }
        Entry::RequiredDir => {
            insert_directory(directories, bytes, target.clone(), DirKind::Required)?;
            add_ancestors(&target, root, directories, bytes, deadline)
        }
    }
}

fn sync_objects(path: &Path, deadline: Instant) -> Result<()> {
    let objects = path.join("objects");
    // The fanout entries are opened through `objects`, so it must be a real directory:
    // a symlink there would redirect every object entry before its own final component check.
    require_directory(&objects, deadline)?;
    let mut directories = BTreeMap::new();
    for name in ["objects/info", "objects/pack"] {
        directories.insert(path.join(name), DirKind::Required);
    }
    // Git names loose object directories with two lowercase hex digits. The set is fixed, so
    // a barrier covers a push's new entries without listing the store.
    for value in 0..=255u8 {
        directories.insert(objects.join(format!("{value:02x}")), DirKind::Optional);
    }
    directories.insert(objects, DirKind::Required);
    // Children before `objects`; the repository root is synced with the ref entries later.
    for (directory, kind) in directories.iter().rev() {
        sync_dir(directory, *kind, deadline)?;
    }
    Ok(())
}

/// Walks each directory component below `root` without following a symlink.
/// `false` means the path cannot exist: a component is missing or a ref file now holds
/// the old namespace. A symlink, or any other non-directory type, is an error.
fn ancestors_are_directories(root: &Path, name: &str, deadline: Instant) -> Result<bool> {
    let components = Path::new(name).iter().collect::<Vec<_>>();
    let Some((_, parents)) = components.split_last() else {
        return Ok(false);
    };
    let mut prefix = root.to_owned();
    for &component in parents {
        ensure!(Instant::now() < deadline, TIMED_OUT);
        prefix.push(component);
        match open_no_follow(&prefix, deadline)? {
            None => return Ok(false),
            Some((_, metadata)) => {
                if metadata.is_dir() {
                    continue;
                }
                ensure!(metadata.is_file(), INVALID);
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Includes empty ref namespaces, which a live-ref listing omits after a deletion.
/// Only the refs tree is inspected; object files are never listed.
fn collect_ref_directories(
    root: &Path,
    directories: &mut BTreeMap<PathBuf, DirKind>,
    bytes: &mut usize,
    deadline: Instant,
) -> Result<()> {
    let refs = root.join("refs");
    insert_directory(directories, bytes, refs.clone(), DirKind::Optional)?;
    let mut pending = vec![refs.clone()];
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        ensure!(Instant::now() < deadline, TIMED_OUT);
        let Some((_, metadata)) = open_no_follow(&directory, deadline)? else {
            continue;
        };
        if !metadata.is_dir() {
            // A concurrent namespace inversion can replace a nested directory with a ref.
            ensure!(directory != refs && metadata.is_file(), INVALID);
            continue;
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            ensure!(Instant::now() < deadline, TIMED_OUT);
            ensure!(
                visited < MAX_SYNC_FILES,
                "Too many repository directory entries"
            );
            visited += 1;
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                let path = entry.path();
                insert_directory(directories, bytes, path.clone(), DirKind::RefAncestor)?;
                pending.push(path);
            } else {
                ensure!(kind.is_file(), INVALID);
            }
        }
    }
    Ok(())
}

fn add_ancestors(
    target: &Path,
    root: &Path,
    directories: &mut BTreeMap<PathBuf, DirKind>,
    bytes: &mut usize,
    deadline: Instant,
) -> Result<()> {
    let mut parent = target.parent();
    while let Some(directory) = parent.filter(|directory| directory.starts_with(root)) {
        ensure!(Instant::now() < deadline, TIMED_OUT);
        insert_directory(
            directories,
            bytes,
            directory.to_owned(),
            DirKind::RefAncestor,
        )?;
        parent = directory.parent();
    }
    Ok(())
}

/// Adds one unique directory to the fsync set, charging its path bytes before it is allocated
/// into the map. A deeply nested ref name expands into many prefixes, so the budget is checked
/// per insertion rather than after the whole set exists.
fn insert_directory(
    directories: &mut BTreeMap<PathBuf, DirKind>,
    bytes: &mut usize,
    directory: PathBuf,
    kind: DirKind,
) -> Result<()> {
    match directories.get_mut(&directory) {
        Some(existing) => {
            *existing = (*existing).max(kind);
            Ok(())
        }
        None => {
            let cost = directory.as_os_str().len();
            ensure!(
                directories.len() < MAX_SYNC_FILES && bytes.saturating_add(cost) <= MAX_SYNC_BYTES,
                "Too many repository directories"
            );
            *bytes += cost;
            directories.insert(directory, kind);
            Ok(())
        }
    }
}

fn parent_directory(path: &Path) -> Option<PathBuf> {
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Some(PathBuf::from(".")),
        Some(parent) => Some(parent.to_owned()),
        None => None,
    }
}

fn sync_file(path: &Path, required: bool, deadline: Instant) -> Result<()> {
    match open_no_follow(path, deadline)? {
        None => ensure!(!required, "Missing repository metadata"),
        Some((file, metadata)) => {
            ensure!(metadata.is_file(), INVALID);
            file.sync_all()?;
        }
    }
    Ok(())
}

/// Requires a real directory without syncing it, so a symlinked parent is refused before
/// anything is opened through it.
fn require_directory(path: &Path, deadline: Instant) -> Result<()> {
    match open_no_follow(path, deadline)? {
        Some((_, metadata)) => ensure!(metadata.is_dir(), INVALID),
        None => bail!("Missing repository directory"),
    }
    Ok(())
}

fn sync_dir(path: &Path, kind: DirKind, deadline: Instant) -> Result<()> {
    match open_no_follow(path, deadline)? {
        None => ensure!(kind != DirKind::Required, "Missing repository directory"),
        Some((file, metadata)) => {
            if metadata.is_dir() {
                file.sync_all()?;
            } else {
                // A ref rename can replace the old nested parent with the ref file itself.
                ensure!(kind == DirKind::RefAncestor, INVALID);
            }
        }
    }
    Ok(())
}

/// Opens one entry without following a final symlink. A missing name is `None`; every other
/// failure, including a symlink, is an error. The caller checks the type, so a regular file
/// where a directory belongs is refused rather than skipped.
fn open_no_follow(
    path: &Path,
    deadline: Instant,
) -> Result<Option<(std::fs::File, std::fs::Metadata)>> {
    use std::os::unix::fs::OpenOptionsExt;

    ensure!(Instant::now() < deadline, TIMED_OUT);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if absent(&error) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    Ok(Some((file, metadata)))
}

/// A name that is gone. A parent component that is now a regular file is deliberately *not*
/// treated as absent here: `ancestors_are_directories` verifies that case as a ref namespace
/// replacement, and anywhere else `ENOTDIR` is an error rather than hidden corruption.
fn absent(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ref_names::MAX_REF_BYTES;

    #[tokio::test]
    async fn inventory_syncs_empty_ref_namespaces_and_refuses_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let path = repository(temporary.path());
        let empty = path.join("refs/heads/deleted/nested");
        std::fs::create_dir_all(&empty).unwrap();
        let mut directories = BTreeMap::new();
        let mut bytes = 0;
        collect_ref_directories(&path, &mut directories, &mut bytes, deadline()).unwrap();
        assert!(directories.contains_key(&empty));
        assert!(directories.contains_key(&path.join("refs/heads/deleted")));
        succeeds(RepositorySync::refs(Vec::new()).inventory(), &path).await;
        std::os::unix::fs::symlink(temporary.path(), path.join("refs/heads/link")).unwrap();
        assert!(collect_ref_directories(&path, &mut directories, &mut bytes, deadline()).is_err());
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    async fn fails(sync: RepositorySync, path: &Path) -> bool {
        sync.sync(path, deadline()).await.is_err()
    }

    async fn succeeds(sync: RepositorySync, path: &Path) {
        sync.sync(path, deadline()).await.unwrap();
    }

    fn repository(root: &Path) -> PathBuf {
        let path = root.join("repository.git");
        std::fs::create_dir_all(path.join("objects/info")).unwrap();
        std::fs::create_dir_all(path.join("objects/pack")).unwrap();
        std::fs::create_dir_all(path.join("refs/heads")).unwrap();
        std::fs::create_dir_all(path.join("refs/tags")).unwrap();
        std::fs::write(path.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(path.join("config"), "").unwrap();
        path
    }

    fn write_ref(path: &Path, name: &str) {
        let file = path.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut line = "1".repeat(40);
        line.push('\n');
        std::fs::write(file, line).unwrap();
    }

    #[tokio::test]
    async fn invalid_names_never_reach_the_filesystem() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        let names = [
            "",
            ".",
            "..",
            "../escape",
            "/etc/passwd",
            "refs/../HEAD",
            "refs/\0x",
        ];
        for name in names {
            assert!(
                fails(RepositorySync::new().optional_file(name), &path).await,
                "{name:?} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn required_metadata_must_exist_and_is_never_followed() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        std::fs::remove_file(path.join("HEAD")).unwrap();
        assert!(fails(RepositorySync::new().required_file("HEAD"), &path).await);
        succeeds(RepositorySync::new().optional_file("shallow"), &path).await;
        std::os::unix::fs::symlink("config", path.join("HEAD")).unwrap();
        assert!(fails(RepositorySync::new().required_file("HEAD"), &path).await);
        std::fs::remove_file(path.join("HEAD")).unwrap();
        std::fs::create_dir(path.join("HEAD")).unwrap();
        assert!(fails(RepositorySync::new().required_file("HEAD"), &path).await);
    }

    #[tokio::test]
    async fn required_directories_must_exist_and_are_never_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        std::fs::remove_dir(path.join("refs/tags")).unwrap();
        assert!(fails(RepositorySync::new().required_dir("refs/tags"), &path).await);
        std::os::unix::fs::symlink("heads", path.join("refs/tags")).unwrap();
        assert!(fails(RepositorySync::new().required_dir("refs/tags"), &path).await);
    }

    #[tokio::test]
    async fn refs_tolerate_deleted_and_directory_replaced_names() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        write_ref(&path, "refs/heads/topic/nested");
        // A rename of `topic` into `topic/nested` leaves the old name as a directory.
        let names = vec![
            "refs/heads/topic".to_owned(),
            "refs/heads/topic/nested".to_owned(),
            "refs/heads/deleted".to_owned(),
            "refs/pull/1/head".to_owned(),
        ];
        succeeds(RepositorySync::refs(names), &path).await;
        // Deleting `refs/heads/moved/nested` and then publishing `refs/heads/moved` inverts
        // the namespace: the old nested name opens ENOTDIR through a verified regular ref
        // file. That is recorded state, while the surviving ref and its parent still need
        // syncing.
        std::fs::write(path.join("refs/heads/moved"), b"ref").unwrap();
        assert!(!ancestors_are_directories(&path, "refs/heads/moved/nested", deadline()).unwrap());
        assert!(ancestors_are_directories(&path, "refs/heads/moved", deadline()).unwrap());
        let moved = vec![
            "refs/heads/moved/nested".to_owned(),
            "refs/heads/moved".to_owned(),
        ];
        succeeds(RepositorySync::refs(moved), &path).await;
        std::os::unix::fs::symlink("/etc/passwd", path.join("refs/heads/linked")).unwrap();
        let linked = vec!["refs/heads/linked".to_owned()];
        assert!(fails(RepositorySync::refs(linked), &path).await);
    }

    #[tokio::test]
    async fn an_inventory_barrier_syncs_every_held_ref() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        write_ref(&path, "refs/heads/topic/nested");
        write_ref(&path, "refs/tags/v1");
        succeeds(
            RepositorySync::refs(Vec::<String>::new()).inventory(),
            &path,
        )
        .await;
    }

    #[tokio::test]
    async fn object_entries_use_the_fixed_fanout_without_walking_object_files() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        // An object file inside a fanout directory is not read; only the entry is synced.
        std::fs::create_dir_all(path.join("objects/ab")).unwrap();
        std::fs::write(path.join("objects/ab/object"), b"object").unwrap();
        succeeds(RepositorySync::new().objects(), &path).await;
        // A regular file where a fanout directory belongs is refused, not skipped.
        std::fs::write(path.join("objects/ff"), b"").unwrap();
        assert!(fails(RepositorySync::new().objects(), &path).await);
    }

    #[tokio::test]
    async fn a_symlinked_ancestor_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        std::fs::create_dir_all(path.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink("../elsewhere", path.join("refs/heads/linked")).unwrap();
        let names = vec!["refs/heads/linked/nested".to_owned()];
        assert!(fails(RepositorySync::refs(names), &path).await);
        // The same rule applies to an ancestor of an explicitly required directory.
        std::fs::remove_dir(path.join("refs/tags")).unwrap();
        std::os::unix::fs::symlink("heads", path.join("refs/tags")).unwrap();
        let sync = RepositorySync::new().required_dir("refs/tags/deep");
        assert!(fails(sync, &path).await);
        // And to the object store: the fanout entries are refused before they are opened
        // through a symlinked `objects`.
        let objects_root = tempfile::tempdir().unwrap();
        let objects_path = repository(objects_root.path());
        std::fs::remove_dir_all(objects_path.join("objects")).unwrap();
        std::os::unix::fs::symlink("../elsewhere", objects_path.join("objects")).unwrap();
        assert!(fails(RepositorySync::new().objects(), &objects_path).await);
    }

    #[test]
    fn the_sync_limit_parser_bounds_configured_and_default_capacity() {
        let default = configured_sync_limit(None);
        assert!((4..=Semaphore::MAX_PERMITS).contains(&default));
        assert_eq!(configured_sync_limit(Some("1")), 1);
        assert_eq!(configured_sync_limit(Some("8")), 8);
        let max = Semaphore::MAX_PERMITS.to_string();
        assert_eq!(configured_sync_limit(Some(&max)), Semaphore::MAX_PERMITS);
        let over = (Semaphore::MAX_PERMITS as u128 + 1).to_string();
        for invalid in ["0", "-1", "abc", "", " 4", "8 ", over.as_str()] {
            assert_eq!(
                configured_sync_limit(Some(invalid)),
                default,
                "{invalid:?} must fall back to the bounded default"
            );
        }
        // The bounded value can be handed to the constructor without a panic.
        let _ = Semaphore::new(configured_sync_limit(Some(&max)));
    }

    #[test]
    fn the_blocking_entry_point_applies_the_same_rule() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        write_ref(&path, "refs/heads/main");
        let names = vec!["refs/heads/main".to_owned()];
        RepositorySync::refs(names)
            .sync_blocking(&path, deadline())
            .unwrap();
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(RepositorySync::new()
            .optional_file(long)
            .sync_blocking(&path, deadline())
            .is_err());
    }

    #[test]
    fn an_unexplained_enotdir_is_an_error_not_an_absent_name() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        std::fs::write(path.join("blocked"), b"file").unwrap();
        assert!(sync_file(&path.join("blocked/child"), false, deadline()).is_err());
        assert!(sync_dir(
            &path.join("blocked/child"),
            DirKind::RefAncestor,
            deadline()
        )
        .is_err());
        assert!(require_directory(&path.join("blocked/child"), deadline()).is_err());
    }

    #[test]
    fn ancestor_budget_stops_a_deep_name_before_it_is_fully_allocated() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let root = Path::new("/repository.git");
        let mut directories = BTreeMap::new();
        let mut bytes = 0usize;
        // Each name is 4000 bytes of one-character components: about 2000 unique prefixes and
        // about 4 MB of path bytes, so a few names exhaust the directory budget.
        let body = "x/".repeat(2_000);
        let mut refused = false;
        for index in 0..8 {
            let target = root.join(format!("{index}{body}"));
            if add_ancestors(&target, root, &mut directories, &mut bytes, deadline).is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "a deep name must exhaust the ancestor budget");
        assert!(bytes <= MAX_SYNC_BYTES);
        assert!(directories.len() <= MAX_SYNC_FILES);
    }

    #[tokio::test]
    async fn a_missing_required_entry_and_a_passed_deadline_are_errors() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        let sync = RepositorySync::new().required_dir("objects/missing");
        assert!(fails(sync, &path).await);
        let past = Instant::now() - Duration::from_secs(1);
        let sync = RepositorySync::new().required_file("HEAD");
        assert!(sync.sync(&path, past).await.is_err());
    }

    #[tokio::test]
    async fn name_count_and_bytes_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let path = repository(root.path());
        let names = (0..=MAX_SYNC_FILES)
            .map(|index| format!("refs/heads/{index}"))
            .collect::<Vec<_>>();
        assert!(fails(RepositorySync::refs(names), &path).await);
        let body = "a".repeat(MAX_NAME_BYTES - 16);
        let names = (0..MAX_SYNC_BYTES / (body.len() + 14) + 2)
            .map(|index| format!("{index:014}{body}"))
            .collect::<Vec<_>>();
        assert!(fails(RepositorySync::refs(names), &path).await);
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(fails(RepositorySync::new().optional_file(long), &path).await);
    }

    /// Deep names at the 1023-byte limit, with distinct first components so the
    /// names do not share their expanded ancestors.
    fn deep_push_names(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| {
                let mut name = format!("refs/heads/{index:02x}/");
                while name.len() + "x/".len() + "y".len() <= MAX_REF_BYTES {
                    name.push_str("x/");
                }
                name.push('y');
                name
            })
            .collect()
    }

    #[tokio::test]
    async fn a_legal_deep_push_is_refused_before_it_touches_the_repository() {
        let root = tempfile::tempdir().unwrap();
        // The path does not exist: the plan is checked without opening the repository.
        let path = root.path().join("repository.git");
        let names = deep_push_names(64);
        assert_eq!(names[0].len(), MAX_REF_BYTES);
        let updates = names
            .iter()
            .map(|reference| crate::protocol::RefUpdate {
                old: crate::protocol::ZERO_SHA.to_owned(),
                new: "1".repeat(40),
                reference: reference.clone(),
            })
            .collect::<Vec<_>>();
        crate::protocol::validate_updates(&updates).unwrap();
        let error = RepositorySync::refs(names)
            .preflight(&path, deadline())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Too many repository directories"),
            "{error}"
        );
        assert!(!path.exists(), "a refused plan must not create anything");
    }

    #[tokio::test]
    async fn shared_ancestors_are_charged_once_and_duplicate_aliases_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repository.git");
        let mut prefix = String::from("refs/heads/shared/");
        while prefix.len() + "x/".len() + "/branch255".len() <= MAX_REF_BYTES {
            prefix.push_str("x/");
        }
        // 256 branches under one deep namespace share every ancestor directory, so the
        // barrier charges the chain once. Charging it per name would refuse this plan.
        let mut names = (0..256)
            .map(|index| format!("{prefix}branch{index}"))
            .collect::<Vec<_>>();
        names.push("HEAD".to_owned());
        for name in names.iter().filter(|name| name.as_str() != "HEAD") {
            assert!(crate::ref_names::is_canonical_ref(name), "{name}");
        }
        let plan = RepositorySync::refs(names);
        plan.preflight_blocking(&path, deadline()).unwrap();
        // The barrier that follows the publication accepts the same plan on a real repository.
        let repository_path = repository(root.path());
        write_ref(&repository_path, "refs/heads/shared/branch0");
        plan.sync(&repository_path, deadline()).await.unwrap();
    }

    #[test]
    fn preflight_requires_a_named_plan_and_never_opens_the_repository() {
        let path = Path::new("/repository-that-does-not-exist.git");
        // A fitting named plan passes on a path that does not exist: nothing is opened.
        RepositorySync::refs(vec!["refs/heads/main".to_owned(), "HEAD".to_owned()])
            .preflight_blocking(path, deadline())
            .unwrap();
        assert!(RepositorySync::new()
            .required_file("../escape")
            .preflight_blocking(path, deadline())
            .is_err());
        assert!(RepositorySync::refs(Vec::<String>::new())
            .inventory()
            .preflight_blocking(path, deadline())
            .is_err());
    }
}
