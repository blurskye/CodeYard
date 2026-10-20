use anyhow::{anyhow, ensure, Context, Result};
use git2::{ObjectType, Repository};
use serde::{Deserialize, Serialize};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;

mod checked;
pub(crate) mod locks;
pub(crate) mod process;
mod replay;
pub(crate) use checked::{
    adds_merge_commit, checkout, commit_is_ancestor, current_pull_head, initialize_repository,
    prepare_merge, prepare_pull_head, prepare_squash, publish_merge, publish_pull_head,
    publish_ref_create, publish_ref_delete, publish_restack, quarantined_is_fast_forward,
    validate_merge_revision, MergeConflict, Person, PreparedPullHead, PullHeadConflict,
    RefConflict, UnsupportedMerge, NO_CHANGES,
};
pub(crate) use replay::replay_commits;

/// Check if a path is a valid git repository
pub(crate) fn is_git_repo(path: &Path) -> bool {
    path.join("HEAD").exists() && path.join("objects").exists() && path.join("refs").exists()
}

/// Whether Git can open `path` as a repository. A path it cannot open as one, including a
/// file where the directory belongs, is `false`, so the caller can rebuild it. A failure to
/// read the path, such as permission denied or a disk error, is an error instead: it says
/// nothing about the repository.
pub(crate) fn opens_as_repository(path: &Path) -> Result<bool> {
    if let Err(error) = std::fs::metadata(path.join("HEAD")) {
        return if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) {
            Ok(false)
        } else {
            Err(error.into())
        };
    }
    match Repository::open_bare(path) {
        Ok(_) => Ok(true),
        Err(error) if error.class() == git2::ErrorClass::Os => Err(error.into()),
        Err(_) => Ok(false),
    }
}

pub const MAX_REPOSITORY_REFS: usize = 10_000;
/// A replica also copies pull request heads, so its list may be longer than the
/// branches and tags that pages show. It still has to fit one RPC reply.
pub const MAX_MIRROR_REFS: usize = 5 * MAX_REPOSITORY_REFS;
pub const MAX_MIRROR_REF_BYTES: usize = 4 * 1024 * 1024;
/// The admission budget for branch and tag names, each with an object ID. The branch
/// list holds names only, so the same budget covers it.
pub const MAX_REPOSITORY_REF_BYTES: usize = 1024 * 1024;
/// Admission is checked without a lock, so concurrent writers can overshoot the admission
/// limits by a few refs. Readers tolerate that margin, so a repository never becomes
/// unreadable, and its refs can still be deleted.
const MAX_READ_REFS: usize = MAX_REPOSITORY_REFS + 256;
const MAX_READ_REF_BYTES: usize = MAX_REPOSITORY_REF_BYTES + 64 * 1024;
const MAX_REF_NAME_BYTES: usize = 4 * 1024;
// Each advertised ref adds a four-byte packet length, a space and a newline to the
// listing budget. Annotated tags also advertise a peeled ref with a three-byte ^{}
// suffix. Reserve another packet for HEAD, capabilities and the terminating flush.
const MAX_REF_ADVERTISEMENT_BYTES: usize = MAX_MIRROR_REF_BYTES
    + MAX_REPOSITORY_REF_BYTES
    + 6 * MAX_MIRROR_REFS
    + 9 * MAX_REPOSITORY_REFS
    + 64 * 1024;
pub const MAX_COMMIT_OBJECT_BYTES: usize = 256 * 1024;
pub const MAX_COMMIT_PAGE_BYTES: usize = 1024 * 1024;
const MAX_COMMIT_WALK_OBJECTS: usize = 200_000;
const MAX_COMMIT_WALK_BYTES: usize = 64 * 1024 * 1024;
/// How long a commit walk may run when its caller sets no shorter deadline.
pub(crate) const COMMIT_WALK_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_TAG_OBJECT_BYTES: usize = 256 * 1024;
const MAX_TAG_DEPTH: usize = 32;
const MAX_TAG_CHAIN_BYTES: usize = 1024 * 1024;
/// Leaves room in a tag object for its target, name and tagger.
pub const MAX_TAG_MESSAGE_BYTES: usize = MAX_TAG_OBJECT_BYTES - 4 * 1024;
pub const MAX_TAG_RESPONSE_BYTES: usize = 1024 * 1024;
/// Largest author or committer name in bytes. Account names hold 255 characters,
/// which is at most 1020 bytes of UTF-8.
pub const MAX_COMMIT_NAME_BYTES: usize = 1024;
/// Largest author or committer email: the no-reply address built from the longest name.
pub const MAX_COMMIT_EMAIL_BYTES: usize = MAX_COMMIT_NAME_BYTES + "@users.noreply".len();

fn parse_branches(output: &[u8]) -> Result<Vec<String>> {
    let mut total_bytes = 0usize;
    let mut branches = Vec::new();
    for branch in output
        .split(|byte| *byte == b'\n')
        .filter(|branch| !branch.is_empty())
    {
        if branches.len() >= MAX_READ_REFS {
            return Err(ObjectTooLarge.into());
        }
        total_bytes = total_bytes
            .checked_add(branch.len())
            .filter(|total| *total <= MAX_READ_REF_BYTES)
            .ok_or(ObjectTooLarge)?;
        branches.push(String::from_utf8_lossy(branch).into_owned());
    }
    Ok(branches)
}

/// The advertised default, without materializing the branch list. Deletion must still
/// work when a repository has more refs than a read response can hold.
pub(crate) async fn default_branch(path: &Path) -> Result<Option<String>> {
    {
        let repo = Repository::open_bare(path)?;
        match repo.head() {
            Ok(head) if head.is_branch() => {
                if head.name_bytes().len() >= MAX_REF_NAME_BYTES {
                    return Err(ObjectTooLarge.into());
                }
                return Ok(head
                    .name_bytes()
                    .strip_prefix(b"refs/heads/")
                    .map(|name| String::from_utf8_lossy(name).into_owned()));
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.code(),
                    git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
                ) => {}
            Err(error) => return Err(error.into()),
        };
    }
    let mut command = process::git_command()?;
    command
        .args([
            "for-each-ref",
            "--sort=refname",
            "--count=1",
            "--format=%(refname:lstrip=2)",
            "refs/heads/",
        ])
        .current_dir(path);
    let (status, output) =
        process::buffered(command, b"", MAX_REF_NAME_BYTES, process::RPC_TIMEOUT).await?;
    ensure!(status.success(), "Git default branch lookup failed");
    let branch = String::from_utf8_lossy(&output)
        .trim_end_matches('\n')
        .to_owned();
    Ok((!branch.is_empty()).then_some(branch))
}

pub(crate) async fn get_repo_info(path: &Path) -> Result<RepoInfo> {
    if !is_git_repo(path) {
        return Err(anyhow!("Not a valid git repository: {:?}", path));
    }

    let mut command = process::git_command()?;
    // `:short` drops only the components needed to be unambiguous, so a tag named like the
    // default branch turns `main` into `heads/main`. `lstrip=2` always removes `refs/heads/`.
    command
        .args(["branch", "--list", "--format=%(refname:lstrip=2)"])
        .current_dir(path);
    let (status, output) =
        process::buffered(command, b"", MAX_READ_REF_BYTES, process::RPC_TIMEOUT).await?;
    ensure!(status.success(), "Git branch listing failed");
    let branches = parse_branches(&output)?;

    let default_branch = default_branch(path).await?.unwrap_or_else(|| "main".into());

    Ok(RepoInfo {
        default_branch,
        branches,
    })
}

pub(crate) async fn count_commits(path: &Path) -> Result<u64> {
    let mut command = process::git_command()?;
    command
        .args(["rev-list", "--count", "--all"])
        .current_dir(path);
    let (status, output) = process::buffered(command, b"", 64, process::RPC_TIMEOUT).await?;
    ensure!(status.success(), "Git commit count failed");
    Ok(std::str::from_utf8(&output)?.trim().parse()?)
}

/// Commits a branch update brought in. A new branch (`old` is None) counts the commits no
/// other ref already had, so the first push to an empty repository counts all of them.
/// `reference` is the branch's full ref name.
pub(crate) async fn count_pushed_commits(
    path: &Path,
    old: Option<&str>,
    new: &str,
    reference: &str,
) -> Result<u64> {
    let mut command = process::git_command()?;
    if let Some(old) = old {
        let range = format!("{old}..{new}");
        command.args(["rev-list", "--count", range.as_str()]);
    } else {
        let exclude = format!("--exclude={reference}");
        command.args([
            "rev-list",
            "--count",
            new,
            "--not",
            exclude.as_str(),
            // Unlike --all, this does not add HEAD, which may name the new branch itself.
            "--glob=refs/*",
        ]);
    }
    command.current_dir(path);
    let (status, output) = process::buffered(command, b"", 64, process::RPC_TIMEOUT).await?;
    ensure!(status.success(), "Git pushed commit count failed");
    Ok(std::str::from_utf8(&output)?.trim().parse()?)
}

pub(crate) fn resolve_archive_revision(repo_path: &Path, git_ref: &str) -> Result<git2::Oid> {
    let repo = Repository::open_bare(repo_path)?;
    let (oid, _) = peel_revision(&repo, git_ref)?;
    Ok(oid)
}

/// The archive formats a tree can be downloaded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
}

/// The tree at `revision` as an archive whose entries sit under `prefix/`. The tar.gz form is
/// gzipped here from Git's tar output, so both formats share the same size limit on the tree.
pub(crate) async fn archive(
    repo_path: &Path,
    revision: git2::Oid,
    prefix: &str,
    format: ArchiveFormat,
    limit: usize,
) -> Result<Vec<u8>> {
    // Only a resolved object ID can reach Git, never an option from user input.
    let revision = revision.to_string();
    let prefix_arg = format!("--prefix={}/", prefix);
    let git_format = match format {
        ArchiveFormat::Zip => "--format=zip",
        ArchiveFormat::TarGz => "--format=tar",
    };
    let mut command = process::git_command()?;
    command
        .args(["archive", git_format, &prefix_arg, &revision])
        .current_dir(repo_path);
    let (status, output) = process::buffered(command, b"", limit, process::RPC_TIMEOUT).await?;
    anyhow::ensure!(status.success(), "Git archive failed");
    match format {
        ArchiveFormat::Zip => Ok(output),
        ArchiveFormat::TarGz => {
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&output)?;
                Ok(encoder.finish()?)
            })
            .await?
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub default_branch: String,
    pub branches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub entry_type: String,
    pub size: Option<u64>,
    pub last_commit: Option<CommitInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobContent {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub is_binary: bool,
}

pub const MAX_BLOB_MATERIALIZE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("repository object exceeds its response limit")]
pub(crate) struct ObjectTooLarge;

#[derive(Debug, thiserror::Error)]
#[error("repository byte range is not satisfiable")]
pub(crate) struct InvalidByteRange;

/// Resolves a path without materializing its blob and returns its object ID and size.
pub(crate) fn blob_metadata(repo_path: &Path, git_ref: &str, path: &str) -> Result<(String, u64)> {
    let repo = Repository::open_bare(repo_path)?;
    let commit = peel_to_commit_checked(&repo, git_ref)?;
    let entry = tree_entry(&repo, &commit, path)?;
    anyhow::ensure!(entry.kind() == Some(ObjectType::Blob), "Not a blob");
    let (size, kind) = repo.odb()?.read_header(entry.id())?;
    anyhow::ensure!(kind == ObjectType::Blob, "Not a blob");
    Ok((entry.id().to_string(), size as u64))
}

/// Reads only the requested part of a known blob. Git is stopped once the range and
/// binary probe are complete, so a small range never buffers the rest of the object.
pub(crate) async fn read_blob_range(
    repo_path: &Path,
    id: &str,
    size: u64,
    offset: u64,
    length: usize,
) -> Result<(Vec<u8>, bool)> {
    use anyhow::Context;
    use tokio::io::AsyncReadExt;

    let end = offset
        .checked_add(length as u64)
        .filter(|end| *end <= size)
        .ok_or(InvalidByteRange)?;
    let probe_end = size.min(8_000);
    let read_end = if length == 0 {
        probe_end
    } else {
        end.max(probe_end)
    };
    if read_end == 0 {
        return Ok((Vec::new(), false));
    }

    let oid = git2::Oid::from_str(id)?;
    let mut command = process::git_command()?;
    command
        .args(["cat-file", "blob", &oid.to_string()])
        .current_dir(repo_path);
    let mut child = command.spawn().context("start git cat-file")?;
    let mut group = process::ProcessGroup(child.id());
    let mut stdout = child.stdout.take().context("git cat-file stdout")?;
    let stderr = child.stderr.take().context("git cat-file stderr")?;
    drop(child.stdin.take());

    let work = async {
        let mut bytes = Vec::with_capacity(length);
        let mut binary = false;
        let mut position = 0u64;
        let mut buffer = vec![0u8; process::CHUNK];
        while position < read_end {
            let wanted = (read_end - position).min(buffer.len() as u64) as usize;
            let count = stdout.read(&mut buffer[..wanted]).await?;
            anyhow::ensure!(count > 0, "Git blob ended before its declared size");
            let chunk_end = position + count as u64;
            if position < probe_end {
                let probe = (probe_end - position).min(count as u64) as usize;
                binary |= buffer[..probe].contains(&0);
            }
            let copy_start = position.max(offset);
            let copy_end = chunk_end.min(end);
            if copy_start < copy_end {
                let from = (copy_start - position) as usize;
                let to = (copy_end - position) as usize;
                bytes.extend_from_slice(&buffer[from..to]);
            }
            position = chunk_end;
        }
        anyhow::ensure!(bytes.len() == length, "Git blob range was incomplete");

        if read_end < size {
            process::kill_group(child.id());
        }
        let status = child.wait().await?;
        let errors = process::read_bounded(stderr, 16 * 1024).await?;
        if read_end == size {
            anyhow::ensure!(
                status.success(),
                "Git blob read failed: {}",
                String::from_utf8_lossy(&errors)
            );
        }
        Ok::<_, anyhow::Error>((bytes, binary))
    };
    let result = tokio::time::timeout(process::RPC_TIMEOUT, work)
        .await
        .context("git blob read timed out")?;
    if result.is_ok() {
        group.0 = None;
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub committer_name: String,
    pub committer_email: String,
    pub timestamp: i64,
    pub committer_timestamp: i64,
    pub parents: Vec<String>,
}

/// Which commits a history lists. The author is matched against "Name <email>", ignoring case,
/// as `git log --author` matches it. The dates are Unix seconds, compared with the committer time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFilter {
    pub author: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Text the commit message must contain, ignoring case.
    pub message: Option<String>,
}

impl CommitFilter {
    /// True when every commit passes. An empty filter is left out of replica requests, so an
    /// older node still answers an unfiltered history.
    pub fn is_empty(&self) -> bool {
        self.author.is_none()
            && self.since.is_none()
            && self.until.is_none()
            && self.message.is_none()
    }

    fn matches(&self, commit: &git2::Commit<'_>) -> bool {
        let time = commit.committer().when().seconds();
        if self.since.is_some_and(|since| time < since)
            || self.until.is_some_and(|until| time > until)
        {
            return false;
        }
        if let Some(message) = &self.message {
            let text = commit.message().unwrap_or("").to_lowercase();
            if !text.contains(&message.to_lowercase()) {
                return false;
            }
        }
        match &self.author {
            None => true,
            Some(author) => {
                let person = commit.author();
                let signature = format!(
                    "{} <{}>",
                    person.name().unwrap_or(""),
                    person.email().unwrap_or("")
                );
                signature.to_lowercase().contains(&author.to_lowercase())
            }
        }
    }
}

/// A branch against the default branch: the commits it has that the default lacks, the commits
/// the default has that it lacks, and when its tip commit was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchStatus {
    pub ahead: usize,
    pub behind: usize,
    pub updated: i64,
}

pub const MAX_TREE_ENTRIES: usize = 1_000;
const MAX_TREE_PATH_BYTES: usize = 4 * 1024;
const MAX_TREE_PATH_TOTAL_BYTES: usize = 128 * 1024;
/// A tree object larger than this cannot pass the entry and name limits.
const MAX_TREE_OBJECT_BYTES: usize = MAX_TREE_PATH_TOTAL_BYTES + MAX_TREE_ENTRIES * 32;
const MAX_TREE_HISTORY_COMMITS: usize = 10_000;
const MAX_TREE_HISTORY_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TREE_COMMIT_FIELD_BYTES: usize = 4 * 1024;
const MAX_TREE_COMMIT_TOTAL_BYTES: usize = 512 * 1024;

/// Finds the directory at `path`, reading each tree on the way only after its size is
/// known. A file, or a tree too large to list, is refused without being loaded.
fn directory_tree<'repo>(
    repo: &'repo Repository,
    commit: &git2::Commit<'repo>,
    path: &str,
) -> Result<git2::Tree<'repo>> {
    fn check_tree_size(repo: &Repository, id: git2::Oid) -> Result<()> {
        let (size, kind) = repo.odb()?.read_header(id)?;
        ensure!(kind == ObjectType::Tree, "Expected tree");
        if size > MAX_TREE_OBJECT_BYTES {
            return Err(ObjectTooLarge.into());
        }
        Ok(())
    }
    check_tree_size(repo, commit.tree_id())?;
    let mut tree = commit.tree()?;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        let missing = |message: &str| {
            git2::Error::new(git2::ErrorCode::NotFound, git2::ErrorClass::Tree, message)
        };
        let (kind, id) = {
            let entry = tree
                .get_name(name)
                .ok_or_else(|| missing("the path does not exist in the given tree"))?;
            (entry.kind(), entry.id())
        };
        if kind != Some(ObjectType::Tree) {
            return Err(missing("the path is not a directory").into());
        }
        check_tree_size(repo, id)?;
        tree = repo.find_tree(id)?;
    }
    Ok(tree)
}

/// Finds the entry at `path` without loading the object it names. The directory holding
/// the entry is found by `directory_tree`, so every tree on the way is size-checked first.
pub(crate) fn tree_entry<'repo>(
    repo: &'repo Repository,
    commit: &git2::Commit<'repo>,
    path: &str,
) -> Result<git2::TreeEntry<'static>> {
    let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
    let tree = directory_tree(repo, commit, directory)?;
    Ok(tree.get_path(Path::new(name))?)
}

fn check_tree_materialization(entry_count: usize, path_bytes: usize) -> Result<()> {
    if entry_count > MAX_TREE_ENTRIES || path_bytes > MAX_TREE_PATH_TOTAL_BYTES {
        return Err(ObjectTooLarge.into());
    }
    Ok(())
}

fn apply_tree_history(output: &[u8], directory: &str, entries: &mut [TreeEntry]) -> Result<()> {
    let by_name: HashMap<Vec<u8>, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.name.as_bytes().to_vec(), index))
        .collect();
    let directory = directory.as_bytes();
    let mut assigned = vec![false; entries.len()];
    let mut remaining = entries.len();
    let mut commit_count = 0usize;
    let mut materialized_bytes = 0usize;
    let mut fields = output.split(|byte| *byte == 0).peekable();

    while let Some(marker) = fields.next() {
        if marker.is_empty() && fields.peek().is_none() {
            break;
        }
        ensure!(marker.is_empty(), "Malformed git log output");
        let sha = fields.next().ok_or_else(|| anyhow!("Missing commit ID"))?;
        let message = fields
            .next()
            .ok_or_else(|| anyhow!("Missing commit subject"))?;
        let author_name = fields
            .next()
            .ok_or_else(|| anyhow!("Missing commit author"))?;
        let author_email = fields
            .next()
            .ok_or_else(|| anyhow!("Missing commit email"))?;
        let timestamp = fields
            .next()
            .ok_or_else(|| anyhow!("Missing commit timestamp"))?;
        let committer_name = fields
            .next()
            .ok_or_else(|| anyhow!("Missing commit committer"))?;
        let committer_email = fields
            .next()
            .ok_or_else(|| anyhow!("Missing commit committer email"))?;
        let committer_timestamp = fields
            .next()
            .ok_or_else(|| anyhow!("Missing commit committer timestamp"))?;
        commit_count += 1;
        if commit_count > MAX_TREE_HISTORY_COMMITS {
            return Err(ObjectTooLarge.into());
        }
        if message.len() > MAX_TREE_COMMIT_FIELD_BYTES
            || author_name.len() > MAX_TREE_COMMIT_FIELD_BYTES
            || author_email.len() > MAX_TREE_COMMIT_FIELD_BYTES
            || committer_name.len() > MAX_TREE_COMMIT_FIELD_BYTES
            || committer_email.len() > MAX_TREE_COMMIT_FIELD_BYTES
        {
            return Err(ObjectTooLarge.into());
        }

        // A merge's combined list follows an empty field, and its first path has no newline
        // before it. A regular commit's list starts with a newline.
        let merge = fields.peek().is_some_and(|field| field.is_empty());
        if merge {
            fields.next();
        }
        let mut first_path = !merge;
        let mut matched = Vec::new();
        while fields.peek().is_some_and(|field| !field.is_empty()) {
            let mut changed = fields.next().expect("peeked path");
            if first_path {
                changed = changed
                    .strip_prefix(b"\n")
                    .ok_or_else(|| anyhow!("Malformed git log path output"))?;
                first_path = false;
            }
            let relative = if directory.is_empty() {
                changed
            } else {
                changed
                    .strip_prefix(directory)
                    .and_then(|path| path.strip_prefix(b"/"))
                    .unwrap_or_default()
            };
            let name = relative
                .split(|byte| *byte == b'/')
                .next()
                .unwrap_or_default();
            if let Some(&index) = by_name.get(name) {
                if !assigned[index] && !matched.contains(&index) {
                    matched.push(index);
                }
            }
        }

        if matched.is_empty() {
            continue;
        }
        ensure!(
            sha.len() == 40 && sha.iter().all(|byte| byte.is_ascii_hexdigit()),
            "Malformed git log commit ID"
        );
        let timestamp = std::str::from_utf8(timestamp)?.parse()?;
        let committer_timestamp = std::str::from_utf8(committer_timestamp)?.parse()?;
        let info = CommitInfo {
            sha: String::from_utf8_lossy(sha).into_owned(),
            message: String::from_utf8_lossy(message).into_owned(),
            author_name: String::from_utf8_lossy(author_name).into_owned(),
            author_email: String::from_utf8_lossy(author_email).into_owned(),
            committer_name: String::from_utf8_lossy(committer_name).into_owned(),
            committer_email: String::from_utf8_lossy(committer_email).into_owned(),
            timestamp,
            committer_timestamp,
            parents: Vec::new(),
        };
        let bytes = info.sha.len()
            + info.message.len()
            + info.author_name.len()
            + info.author_email.len()
            + info.committer_name.len()
            + info.committer_email.len();
        materialized_bytes = bytes
            .checked_mul(matched.len())
            .and_then(|bytes| materialized_bytes.checked_add(bytes))
            .filter(|bytes| *bytes <= MAX_TREE_COMMIT_TOTAL_BYTES)
            .ok_or(ObjectTooLarge)?;
        for index in matched {
            entries[index].last_commit = Some(info.clone());
            assigned[index] = true;
            remaining -= 1;
        }
        if remaining == 0 {
            break;
        }
    }
    // History is optional: entries such as empty trees need not have a changed path.
    Ok(())
}

/// Serialized paths stay below the RPC JSON frame limit, including JSON escaping.
const MAX_FILE_LIST_BYTES: usize = 1024 * 1024;

/// The file paths at a revision, sorted. `truncated` is set when there were more than
/// `limit`; the first `limit` paths in sorted order are kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileList {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// Every file path at `git_ref`. `ls-tree` without `-l` reads tree objects and names only,
/// never blob contents or sizes.
pub(crate) async fn list_file_paths(
    repo_path: &Path,
    git_ref: &str,
    limit: usize,
) -> Result<FileList> {
    let revision = {
        let repo = Repository::open_bare(repo_path)?;
        let commit = peel_to_commit_checked(&repo, git_ref)?;
        commit.id().to_string()
    };
    let mut command = process::git_command()?;
    command
        .args(["ls-tree", "-r", "--name-only", "-z", &revision, "--"])
        .current_dir(repo_path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let _permit = process::operation_slot(process::RPC_TIMEOUT).await?;
    let mut child = command.spawn().context("start git")?;
    let mut group = process::ProcessGroup(child.id());
    let mut reader = BufReader::new(child.stdout.take().context("git stdout")?);
    let work = async {
        let mut paths = BinaryHeap::<String>::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut cutoff: Option<String> = None;
        loop {
            let mut path = Vec::new();
            loop {
                let chunk = reader.fill_buf().await?;
                if chunk.is_empty() {
                    break;
                }
                let end = chunk.iter().position(|byte| *byte == 0);
                let used = end.map_or(chunk.len(), |at| at + 1);
                if path.len() + used > MAX_FILE_LIST_BYTES {
                    return Err(ObjectTooLarge.into());
                }
                path.extend_from_slice(&chunk[..used]);
                reader.consume(used);
                if end.is_some() {
                    break;
                }
            }
            if path.is_empty() {
                break;
            }
            ensure!(path.pop() == Some(0), "invalid Git file listing");
            let path = String::from_utf8_lossy(&path).into_owned();
            if cutoff.as_ref().is_some_and(|cutoff| path >= *cutoff) {
                truncated = true;
                continue;
            }
            bytes += serde_json::to_string(&path)?.len() + 1;
            paths.push(path);
            // Keep the sorted prefix even when tree order differs from string order.
            while paths.len() > limit || bytes > MAX_FILE_LIST_BYTES {
                let path = paths.pop().expect("a path exceeds the budget");
                bytes -= serde_json::to_string(&path)?.len() + 1;
                cutoff = Some(path);
                truncated = true;
            }
        }
        ensure!(child.wait().await?.success(), "Git file listing failed");
        group.0 = None;
        Ok::<_, anyhow::Error>(FileList {
            paths: paths.into_sorted_vec(),
            truncated,
        })
    };
    tokio::time::timeout(process::RPC_TIMEOUT, work)
        .await
        .context("Git file listing timed out")?
}

pub(crate) async fn list_tree(
    repo_path: &Path,
    git_ref: &str,
    path: &str,
) -> Result<Vec<TreeEntry>> {
    list_tree_with_history(repo_path, git_ref, path, true).await
}

/// Sidebar folders need names and kinds only, with one budget for the whole panel.
pub(crate) async fn list_sidebar_trees(
    repo_path: &Path,
    git_ref: &str,
    paths: &[String],
) -> Result<Vec<(String, Vec<TreeEntry>)>> {
    let mut folders = Vec::new();
    let mut count = 0;
    let mut bytes = 0;
    for path in paths.iter().take(256) {
        let entries = match list_tree_with_history(repo_path, git_ref, path, false).await {
            Ok(entries) => entries,
            Err(error) if error.is::<ObjectTooLarge>() => continue,
            Err(error) => return Err(error),
        };
        count += entries.len();
        bytes += entries
            .iter()
            .map(|entry| entry.path.len() + entry.name.len())
            .sum::<usize>();
        if count > 5_000 || bytes > 256 * 1024 {
            break;
        }
        folders.push((path.clone(), entries));
    }
    Ok(folders)
}

pub(crate) async fn list_tree_with_history(
    repo_path: &Path,
    git_ref: &str,
    path: &str,
    history: bool,
) -> Result<Vec<TreeEntry>> {
    let (commit_sha, mut entries) = {
        let (repo_path, git_ref, path) =
            (repo_path.to_owned(), git_ref.to_owned(), path.to_owned());
        let permit = process::operation_slot(process::RPC_TIMEOUT).await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let repo = Repository::open_bare(&repo_path)?;
            let commit = peel_to_commit_checked(&repo, &git_ref)?;
            let commit_sha = commit.id().to_string();
            let target_tree = directory_tree(&repo, &commit, &path)?;
            check_tree_materialization(target_tree.len(), 0)?;

            let odb = repo.odb()?;
            let mut total_path_bytes = 0usize;
            let mut entries = Vec::with_capacity(target_tree.len());
            for entry in target_tree.iter() {
                let name = String::from_utf8_lossy(entry.name_bytes()).into_owned();
                let entry_path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                if entry_path.len() > MAX_TREE_PATH_BYTES {
                    return Err(ObjectTooLarge.into());
                }
                total_path_bytes = total_path_bytes
                    .checked_add(entry_path.len())
                    .ok_or(ObjectTooLarge)?;
                check_tree_materialization(entries.len() + 1, total_path_bytes)?;

                let (entry_type, size) = match entry.kind() {
                    Some(ObjectType::Blob) => {
                        let (size, kind) = odb.read_header(entry.id())?;
                        ensure!(kind == ObjectType::Blob, "Expected blob");
                        ("file".to_string(), Some(size as u64))
                    }
                    Some(ObjectType::Tree) => ("dir".to_string(), None),
                    _ => ("unknown".to_string(), None),
                };
                entries.push(TreeEntry {
                    name,
                    path: entry_path,
                    entry_type,
                    size,
                    last_commit: None,
                });
            }
            Ok::<_, anyhow::Error>((commit_sha, entries))
        })
        .await
        .context("Git tree read stopped")??
    };

    if history && !entries.is_empty() {
        // `-c` lists the paths that differ from every parent, so a file that only a merge
        // introduced gets that merge as its last commit. `--diff-merges` needs git 2.31.
        let mut command = process::git_command()?;
        command
            .arg("--literal-pathspecs")
            .arg("log")
            .arg(format!("--max-count={}", MAX_TREE_HISTORY_COMMITS + 1))
            .arg("--format=%x00%H%x00%s%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct")
            .arg("--name-only")
            .arg("-c")
            .arg("-z")
            .arg(&commit_sha)
            .arg("--");
        for entry in &entries {
            command.arg(&entry.path);
        }
        command.current_dir(repo_path);
        // History is optional enrichment: exhausted budgets leave unresolved entries
        // empty, while Git failures and malformed output still fail the read.
        match process::buffered(
            command,
            b"",
            MAX_TREE_HISTORY_OUTPUT_BYTES,
            process::RPC_TIMEOUT,
        )
        .await
        {
            Ok((status, output)) => {
                ensure!(status.success(), "Git tree history failed");
                if let Err(error) = apply_tree_history(&output, path, &mut entries) {
                    if !error.is::<ObjectTooLarge>() {
                        return Err(error);
                    }
                }
            }
            Err(error) if error.is::<process::OutputLimitExceeded>() => {}
            Err(error) => return Err(error),
        }
    }

    entries.sort_by(|a, b| match (&a.entry_type[..], &b.entry_type[..]) {
        ("dir", "file") => std::cmp::Ordering::Less,
        ("file", "dir") => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    Ok(entries)
}

#[cfg(test)]
pub(crate) fn get_blob_bytes(
    repo_path: &Path,
    git_ref: &str,
    path: &str,
) -> Result<(Vec<u8>, bool)> {
    let repo = Repository::open_bare(repo_path)?;

    let obj = repo.revparse_single(git_ref)?;
    let commit = obj.peel_to_commit()?;
    let tree = commit.tree()?;

    let entry = tree.get_path(Path::new(path))?;
    let (size, kind) = repo.odb()?.read_header(entry.id())?;
    anyhow::ensure!(kind == ObjectType::Blob, "Not a blob");
    if size > MAX_BLOB_MATERIALIZE_BYTES {
        return Err(ObjectTooLarge.into());
    }
    let obj = entry.to_object(&repo)?;
    let blob = obj.as_blob().ok_or_else(|| anyhow!("Not a blob"))?;

    Ok((blob.content().to_vec(), blob.is_binary()))
}

/// A blob by object ID, wherever it appears in the repository.
pub(crate) fn get_blob_by_id(repo_path: &Path, id: &str) -> Result<Vec<u8>> {
    let repo = Repository::open_bare(repo_path)?;
    let id = git2::Oid::from_str(id)?;
    let (size, kind) = repo.odb()?.read_header(id)?;
    anyhow::ensure!(kind == ObjectType::Blob, "Not a blob");
    if size > MAX_BLOB_MATERIALIZE_BYTES {
        return Err(ObjectTooLarge.into());
    }
    let blob = repo.find_blob(id)?;
    Ok(blob.content().to_vec())
}

/// Blame bounds the selected file, subprocess output, memory and execution time.
pub const MAX_BLAME_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_BLAME_LINES: usize = 20_000;
const MAX_BLAME_SUMMARY_BYTES: usize = 4 * 1024;
const MAX_BLAME_METADATA_BYTES: usize = 1024 * 1024;
const BLAME_TOO_LARGE: &str = "This file is too large to blame.";
const BLAME_BINARY: &str = "Binary files cannot be blamed.";

/// A file that blame refuses. The message is the one the page shows.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct BlameRefused(pub(crate) &'static str);

/// One run of consecutive lines last changed by the same commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameHunk {
    /// The first line of the run, counting from 1.
    pub first_line: usize,
    pub line_count: usize,
    pub commit: String,
    pub author_name: String,
    pub author_time: i64,
    /// The subject of the commit message.
    pub summary: String,
}

/// A text file at a revision, with the commit that last changed each run of its lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBlame {
    pub size: u64,
    pub content: String,
    pub hunks: Vec<BlameHunk>,
}

/// Blames a text file as of `git_ref`. The content and the hunks come from the same commit,
/// so the lines always match the hunks.
pub(crate) async fn blame_file(repo_path: &Path, git_ref: &str, path: &str) -> Result<FileBlame> {
    let (revision, size, content) = {
        let (repo_path, git_ref, path) =
            (repo_path.to_owned(), git_ref.to_owned(), path.to_owned());
        let permit = process::operation_slot(process::RPC_TIMEOUT).await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let repo = Repository::open_bare(&repo_path)?;
            let commit = peel_to_commit_checked(&repo, &git_ref)?;
            let entry = tree_entry(&repo, &commit, &path)?;
            let (size, kind) = repo.odb()?.read_header(entry.id())?;
            ensure!(kind == ObjectType::Blob, "Not a blob");
            if size > MAX_BLAME_FILE_BYTES {
                return Err(BlameRefused(BLAME_TOO_LARGE).into());
            }
            let blob = repo.find_blob(entry.id())?;
            if blob.is_binary() {
                return Err(BlameRefused(BLAME_BINARY).into());
            }
            let content = String::from_utf8_lossy(blob.content()).into_owned();
            if content.lines().count() > MAX_BLAME_LINES {
                return Err(BlameRefused(BLAME_TOO_LARGE).into());
            }
            Ok::<_, anyhow::Error>((commit.id().to_string(), size as u64, content))
        })
        .await
        .context("Git blame file read stopped")??
    };
    let mut command = process::git_command()?;
    command
        .arg("--literal-pathspecs")
        .args(["blame", "--incremental", &revision, "--", path])
        .current_dir(repo_path);
    // Historical blobs are untrusted too. Keep their allocation in a killable child with
    // an address-space limit, not in libgit2 on the server's blocking thread pool.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 512 * 1024 * 1024,
                rlim_max: 512 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let (status, output) = process::buffered(
        command,
        b"",
        4 * MAX_BLAME_METADATA_BYTES,
        COMMIT_WALK_TIMEOUT,
    )
    .await?;
    if !status.success() {
        return Err(BlameRefused("Blame exceeded its history or memory limit.").into());
    }
    let hunks = parse_blame(&output)?;
    let mut next = 1usize;
    for hunk in &hunks {
        ensure!(hunk.first_line == next, "Invalid Git blame line range");
        next = next.checked_add(hunk.line_count).ok_or(ObjectTooLarge)?;
    }
    ensure!(
        next - 1 == content.lines().count(),
        "Incomplete Git blame output"
    );
    Ok(FileBlame {
        size,
        content,
        hunks,
    })
}

fn parse_blame(output: &[u8]) -> Result<Vec<BlameHunk>> {
    let text = std::str::from_utf8(output)?;
    let mut authors = HashMap::<String, (String, i64, String)>::new();
    let mut current = None;
    let mut hunks = Vec::new();
    let mut metadata_bytes = 0usize;
    for line in text.lines() {
        if line.len() > 40
            && line.as_bytes()[40] == b' '
            && git2::Oid::from_str(&line[..40]).is_ok()
        {
            let fields: Vec<_> = line.split(' ').collect();
            ensure!(fields.len() == 4, "Invalid Git blame header");
            let first_line = fields[2].parse::<usize>()?;
            let line_count = fields[3].parse::<usize>()?;
            ensure!(
                line_count > 0 && hunks.len() < MAX_BLAME_LINES,
                "Invalid Git blame hunk"
            );
            current = Some((fields[0].to_owned(), first_line, line_count));
            authors.entry(fields[0].to_owned()).or_default();
            continue;
        }
        let (id, first_line, line_count) = current.as_ref().context("Missing Git blame header")?;
        let (author, time, summary) = authors.get_mut(id).expect("blame author");
        if let Some(value) = line.strip_prefix("author ") {
            *author = value.to_owned();
        }
        if let Some(value) = line.strip_prefix("author-time ") {
            *time = value.parse()?;
        }
        if let Some(value) = line.strip_prefix("summary ") {
            if value.len() > MAX_BLAME_SUMMARY_BYTES {
                return Err(ObjectTooLarge.into());
            }
            *summary = value.to_owned();
        }
        if line.starts_with("filename ") {
            metadata_bytes = metadata_bytes
                .checked_add(author.len() + summary.len() + 40)
                .filter(|bytes| *bytes <= MAX_BLAME_METADATA_BYTES)
                .ok_or(ObjectTooLarge)?;
            hunks.push(BlameHunk {
                first_line: *first_line,
                line_count: *line_count,
                commit: id.clone(),
                author_name: author.clone(),
                author_time: *time,
                summary: summary.clone(),
            });
            current = None;
        }
    }
    ensure!(current.is_none(), "Incomplete Git blame hunk");
    hunks.sort_by_key(|hunk| hunk.first_line);
    Ok(hunks)
}

/// Branches and tags, which is what pages and the API show.
pub(crate) fn list_refs(repo_path: &Path) -> Result<HashMap<String, String>> {
    collect_refs(
        repo_path,
        |name| name.starts_with("refs/heads/") || name.starts_with("refs/tags/"),
        MAX_READ_REFS,
        MAX_READ_REF_BYTES,
    )
}

/// What a ref adds to the listings that pages and replicas read: its name and an object ID.
pub(crate) fn ref_listing_bytes(name: &str) -> usize {
    name.len() + 40
}

/// How many branches and tags a repository holds and what they take in those listings.
pub(crate) struct RefsInUse {
    pub count: usize,
    pub bytes: usize,
}

/// Counts every branch and tag for the admission check.
pub(crate) fn branches_and_tags_in_use(repo_path: &Path) -> Result<RefsInUse> {
    refs_in_use(repo_path, &["refs/heads/*", "refs/tags/*"])
}

/// Counts every pull request head for the admission check. Replicas copy them too.
pub(crate) fn pull_heads_in_use(repo_path: &Path) -> Result<RefsInUse> {
    refs_in_use(repo_path, &["refs/pull/*"])
}

/// Counts the refs that match `patterns`. There is no limit here, so a repository past
/// the limits still counts in full.
fn refs_in_use(repo_path: &Path, patterns: &[&str]) -> Result<RefsInUse> {
    let repo = Repository::open_bare(repo_path)?;
    let mut used = RefsInUse { count: 0, bytes: 0 };
    for pattern in patterns {
        for reference in repo.references_glob(pattern)? {
            used.count += 1;
            used.bytes += reference?.name_bytes().len() + 40;
        }
    }
    Ok(used)
}

/// Every ref the repository stores, including pull request heads. A replica copies these.
pub(crate) fn list_mirror_refs(repo_path: &Path) -> Result<HashMap<String, String>> {
    collect_refs(
        repo_path,
        |name| name.starts_with("refs/"),
        MAX_MIRROR_REFS,
        MAX_MIRROR_REF_BYTES,
    )
}

fn collect_refs(
    repo_path: &Path,
    wanted: fn(&str) -> bool,
    max_refs: usize,
    max_bytes: usize,
) -> Result<HashMap<String, String>> {
    let repo = Repository::open_bare(repo_path)?;
    let mut refs = HashMap::new();
    let mut total_bytes = 0usize;

    for reference_result in repo.references()? {
        let reference = reference_result?;
        let Some(name) = reference.name() else {
            continue;
        };

        if !wanted(name) {
            continue;
        }

        if let Some(target) = reference.target() {
            if refs.len() >= max_refs || name.len() > MAX_REF_NAME_BYTES {
                return Err(ObjectTooLarge.into());
            }
            total_bytes = total_bytes
                .checked_add(ref_listing_bytes(name))
                .filter(|total| *total <= max_bytes)
                .ok_or(ObjectTooLarge)?;
            refs.insert(name.to_owned(), target.to_string());
        }
    }

    Ok(refs)
}

pub(crate) async fn advertise_service_refs(repo_path: &Path, service: &str) -> Result<Vec<u8>> {
    let args = match service {
        "upload-pack" => vec!["upload-pack", "--stateless-rpc", "--advertise-refs"],
        "receive-pack" => vec!["receive-pack", "--stateless-rpc", "--advertise-refs"],
        _ => return Err(anyhow!("Unsupported git service '{}'", service)),
    };

    let mut command = process::git_command()?;
    command.args(args).arg(repo_path);
    let (status, output) = process::buffered(
        command,
        b"",
        MAX_REF_ADVERTISEMENT_BYTES,
        process::RPC_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(status.success(), "Git ref advertisement failed");
    Ok(output)
}

fn add_commit_to_budget(total: &mut usize, size: usize, page: bool) -> Result<()> {
    if size > MAX_COMMIT_OBJECT_BYTES {
        return Err(ObjectTooLarge.into());
    }
    if page {
        *total = total
            .checked_add(size)
            .filter(|total| *total <= MAX_COMMIT_PAGE_BYTES)
            .ok_or(ObjectTooLarge)?;
    }
    Ok(())
}

fn commit_object_size(repo: &Repository, oid: git2::Oid) -> Result<usize> {
    let (size, kind) = repo.odb()?.read_header(oid)?;
    ensure!(kind == ObjectType::Commit, "Expected commit");
    Ok(size)
}

/// Refuses a commit or tag over its size limit and returns its size and type. The size
/// comes from the object header, so an oversized object is never read into memory.
fn check_object_size(repo: &Repository, oid: git2::Oid) -> Result<(usize, ObjectType)> {
    let (size, kind) = repo.odb()?.read_header(oid)?;
    let limit = match kind {
        ObjectType::Commit => MAX_COMMIT_OBJECT_BYTES,
        ObjectType::Tag => MAX_TAG_OBJECT_BYTES,
        _ => return Ok((size, kind)),
    };
    if size > limit {
        return Err(ObjectTooLarge.into());
    }
    Ok((size, kind))
}

/// Resolves a revision to an object ID without loading the object. Only full or unique
/// abbreviated IDs and ref names are accepted. An expression such as `main~1`, `main:src`
/// or `tag^{commit}` is an unknown revision: libgit2 would load the object it names before
/// the caller could check that object's size.
fn resolve_revision(repo: &Repository, revision: &str) -> Result<git2::Oid> {
    if revision.len() == 40 {
        if let Ok(oid) = git2::Oid::from_str(revision) {
            if repo.odb()?.exists(oid) {
                return Ok(oid);
            }
        }
    }
    // A name that starts with `refs/` is a full ref name and is looked up exactly. A short name
    // prefers a branch over a tag with the same name, where Git's own rule would pick the tag.
    let reference = if revision.starts_with("refs/") {
        repo.find_reference(revision)
    } else {
        repo.find_reference(&format!("refs/heads/{revision}"))
            .or_else(|_| repo.resolve_reference_from_short_name(revision))
    };
    if let Some(oid) = reference
        .ok()
        .and_then(|reference| reference.resolve().ok())
        .and_then(|direct| direct.target())
    {
        return Ok(oid);
    }
    let abbreviated = (4..40).contains(&revision.len());
    if abbreviated && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let padded = git2::Oid::from_str(&format!("{revision:0<40}"))?;
        let oid = repo.odb()?.exists_prefix(padded, revision.len())?;
        return Ok(oid);
    }
    Err(git2::Error::new(
        git2::ErrorCode::NotFound,
        git2::ErrorClass::Reference,
        format!("revspec '{revision}' not found"),
    )
    .into())
}

/// Peels a revision through annotated tags to the object it names, returned with its
/// type. Tag depth and total bytes are bounded before libgit2 reads each tag.
fn peel_revision(repo: &Repository, revision: &str) -> Result<(git2::Oid, ObjectType)> {
    let mut oid = resolve_revision(repo, revision)?;
    let mut depth = 0usize;
    let mut tag_bytes = 0usize;
    loop {
        match check_object_size(repo, oid)? {
            (size, ObjectType::Tag) => {
                if depth >= MAX_TAG_DEPTH {
                    return Err(ObjectTooLarge.into());
                }
                tag_bytes = tag_bytes
                    .checked_add(size)
                    .filter(|bytes| *bytes <= MAX_TAG_CHAIN_BYTES)
                    .ok_or(ObjectTooLarge)?;
                depth += 1;
                oid = repo.find_tag(oid)?.target_id();
            }
            (_, kind) => return Ok((oid, kind)),
        }
    }
}

/// Peels a revision to a commit. An oversized commit or tag is refused before it is read.
pub(crate) fn peel_to_commit_checked<'repo>(
    repo: &'repo Repository,
    revision: &str,
) -> Result<git2::Commit<'repo>> {
    match peel_revision(repo, revision)? {
        (oid, ObjectType::Commit) => Ok(repo.find_commit(oid)?),
        _ => Err(not_a_commit()),
    }
}

/// Loads a commit by ID after checking its size. Use this instead of `find_commit`.
pub(crate) fn find_commit_checked<'repo>(
    repo: &'repo Repository,
    oid: git2::Oid,
) -> Result<git2::Commit<'repo>> {
    match check_object_size(repo, oid)? {
        (_, ObjectType::Commit) => Ok(repo.find_commit(oid)?),
        _ => Err(not_a_commit()),
    }
}

/// libgit2 reports a revision that is not a commit as an invalid spec, and callers map
/// that kind to not found. Keep the same kind here.
fn not_a_commit() -> anyhow::Error {
    git2::Error::new(
        git2::ErrorCode::InvalidSpec,
        git2::ErrorClass::Object,
        "Object is not a commit",
    )
    .into()
}

/// True for the error above: the revision names a blob or a tree.
pub(crate) fn is_not_a_commit(error: &anyhow::Error) -> bool {
    error.downcast_ref::<git2::Error>().is_some_and(|error| {
        error.code() == git2::ErrorCode::InvalidSpec && error.class() == git2::ErrorClass::Object
    })
}

/// Up to `limit` commits after `offset` that pass `filter`, newest first.
pub(crate) fn list_commits(
    repo_path: &Path,
    git_ref: &str,
    offset: usize,
    limit: usize,
    filter: &CommitFilter,
    deadline: Instant,
) -> Result<Vec<CommitInfo>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let repo = Repository::open_bare(repo_path)?;
    let tip = peel_to_commit_checked(&repo, git_ref)?;
    let mut pending = BinaryHeap::from([(tip.time().seconds(), tip.id())]);
    let mut seen = HashSet::from([tip.id()]);
    let mut walked_bytes = commit_object_size(&repo, tip.id())?;
    let mut total_bytes = 0usize;
    let mut position = 0usize;
    let mut commits = Vec::new();

    // libgit2's time sort eagerly loads the entire history. Visit only the frontier,
    // newest first, and check every object before loading it, including skipped rows.
    while let Some((_, oid)) = pending.pop() {
        ensure!(Instant::now() < deadline, "Git commit walk timed out");
        let commit = find_commit_checked(&repo, oid)?;
        if filter.matches(&commit) {
            if position >= offset {
                add_commit_to_budget(&mut total_bytes, commit_object_size(&repo, oid)?, true)?;
                commits.push(commit_info(&commit));
                if commits.len() == limit {
                    break;
                }
            }
            position += 1;
        }
        for parent in commit.parent_ids() {
            ensure!(Instant::now() < deadline, "Git commit walk timed out");
            if !seen.insert(parent) {
                continue;
            }
            if seen.len() > MAX_COMMIT_WALK_OBJECTS {
                return Err(ObjectTooLarge.into());
            }
            let size = commit_object_size(&repo, parent)?;
            add_commit_to_budget(&mut total_bytes, size, false)?;
            walked_bytes = walked_bytes
                .checked_add(size)
                .filter(|total| *total <= MAX_COMMIT_WALK_BYTES)
                .ok_or(ObjectTooLarge)?;
            let parent = repo.find_commit(parent)?;
            pending.push((parent.time().seconds(), parent.id()));
        }
    }
    Ok(commits)
}

/// The commits reachable from `head` but not from `base`, newest first, at most `limit` of them.
/// These are the commits of a pull request. Both revisions are peeled to commit IDs first, so a
/// ref name is never read as an option.
pub(crate) fn list_range_commits(
    repo_path: &Path,
    base: &str,
    head: &str,
    limit: usize,
) -> Result<Vec<CommitInfo>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let repo = Repository::open_bare(repo_path)?;
    let base = peel_to_commit_checked(&repo, base)?.id();
    let head = peel_to_commit_checked(&repo, head)?.id();
    let deadline = Instant::now() + COMMIT_WALK_TIMEOUT;
    let mut walked_bytes = 0usize;
    let mut checked = HashSet::new();
    let mut load = |oid| -> Result<git2::Commit<'_>> {
        ensure!(Instant::now() < deadline, "Git commit walk timed out");
        if checked.insert(oid) {
            if checked.len() > MAX_COMMIT_WALK_OBJECTS {
                return Err(ObjectTooLarge.into());
            }
            let size = commit_object_size(&repo, oid)?;
            if size > MAX_COMMIT_OBJECT_BYTES {
                return Err(ObjectTooLarge.into());
            }
            walked_bytes = walked_bytes
                .checked_add(size)
                .filter(|bytes| *bytes <= MAX_COMMIT_WALK_BYTES)
                .ok_or(ObjectTooLarge)?;
        }
        Ok(repo.find_commit(oid)?)
    };
    let mut hidden = HashSet::new();
    let mut ancestors = vec![base];
    while let Some(oid) = ancestors.pop() {
        if hidden.insert(oid) {
            ancestors.extend(load(oid)?.parent_ids());
        }
    }
    let tip = load(head)?;
    let mut pending = BinaryHeap::from([(tip.time().seconds(), head)]);
    let mut seen = HashSet::from([head]);
    let mut commits = Vec::new();
    let mut total_bytes = 0usize;
    while let Some((_, oid)) = pending.pop() {
        if hidden.contains(&oid) {
            continue;
        }
        let commit = load(oid)?;
        add_commit_to_budget(&mut total_bytes, commit_object_size(&repo, oid)?, true)?;
        commits.push(commit_info(&commit));
        if commits.len() == limit {
            break;
        }
        for parent in commit.parent_ids() {
            if !hidden.contains(&parent) && seen.insert(parent) {
                let parent = load(parent)?;
                pending.push((parent.time().seconds(), parent.id()));
            }
        }
    }
    Ok(commits)
}

pub(crate) fn get_commit(repo_path: &Path, sha: &str) -> Result<CommitInfo> {
    let repo = Repository::open_bare(repo_path)?;
    let commit = peel_to_commit_checked(&repo, sha)?;
    Ok(commit_info(&commit))
}

/// Enough for one page of commit IDs, which is far below this limit.
/// A line of a file that contains the text a code search looks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeMatch {
    pub path: String,
    pub line_number: u32,
    pub line: String,
}

const CODE_SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
/// A match shows at most this many characters of its line.
const MAX_CODE_LINE_CHARS: usize = 200;
/// The bytes kept from one `git grep` record. A record holds a path, a line number and the whole
/// line, so one very long line keeps only its start.
const MAX_GREP_RECORD_BYTES: usize = 16 * 1024;

/// Up to `limit` lines of the files at `git_ref` that contain `text`, ignoring case, after the first
/// `offset` of them. Binary files are skipped. The text is a literal given
/// after `-e`. Git's output is read as it is printed, and git is stopped once the page is full, so
/// many or very long matching lines cannot fail the search.
pub(crate) async fn search_code(
    repo_path: &Path,
    git_ref: &str,
    text: &str,
    offset: usize,
    limit: usize,
) -> Result<Vec<CodeMatch>> {
    if limit == 0 || text.is_empty() {
        return Ok(Vec::new());
    }
    let tip = {
        let repo = Repository::open_bare(repo_path)?;
        let commit = peel_to_commit_checked(&repo, git_ref)?;
        commit.id().to_string()
    };
    let mut command = process::git_command()?;
    command
        .args(["grep", "-I", "-n", "-i", "-F", "-z", "-e"])
        .arg(text)
        .arg(&tip)
        .arg("--")
        .current_dir(repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let _permit = process::operation_slot(process::RPC_TIMEOUT).await?;
    let mut child = command.spawn().context("start git")?;
    // Kills git's process group if the search returns before git has finished.
    let mut group = process::ProcessGroup(child.id());
    let stdout = child.stdout.take().context("git stdout")?;
    let prefix = format!("{tip}:");
    let mut records = BufReader::new(stdout);
    let mut matches = Vec::new();
    let mut skipped = 0;
    let read = async {
        while let Some(record) = read_grep_record(&mut records).await? {
            let Some(found) = parse_code_match(&record, &prefix) else {
                continue;
            };
            if skipped < offset {
                skipped += 1;
            } else {
                matches.push(found);
                if matches.len() == limit {
                    break;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(CODE_SEARCH_TIMEOUT, read)
        .await
        .context("git timed out")??;
    if matches.len() < limit {
        // Git printed every record, so its status tells whether the search ran. Status 1 means no
        // line matched.
        let status = child.wait().await?;
        group.0 = None;
        ensure!(
            status.success() || status.code() == Some(1),
            "Git could not search the code"
        );
    }
    Ok(matches)
}

/// The next record of `git grep -z -n` output, which ends at a newline. Only the first
/// `MAX_GREP_RECORD_BYTES` of a record are kept; the rest of a long line is read and dropped.
async fn read_grep_record(reader: &mut BufReader<ChildStdout>) -> Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok((!record.is_empty()).then_some(record));
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let used = newline.map_or(chunk.len(), |at| at + 1);
        let room = MAX_GREP_RECORD_BYTES - record.len();
        record.extend_from_slice(&chunk[..used.min(room)]);
        reader.consume(used);
        if newline.is_some() {
            return Ok(Some(record));
        }
    }
}

/// The match in one record, which holds `path`, a line number and the line. Git writes the path
/// after the revision and a colon, and this strips that prefix.
fn parse_code_match(record: &[u8], prefix: &str) -> Option<CodeMatch> {
    let record = String::from_utf8_lossy(record);
    let mut parts = record.splitn(3, '\0');
    let path = parts.next()?.strip_prefix(prefix)?;
    let line_number = parts.next()?.parse().ok()?;
    let text = parts.next()?;
    Some(CodeMatch {
        path: path.to_owned(),
        line_number,
        line: text.trim().chars().take(MAX_CODE_LINE_CHARS).collect(),
    })
}

const MAX_PATH_COMMIT_OUTPUT_BYTES: usize = 64 * 1024;

/// The commits after `offset` that changed `path`, newest first, as `git log -- <path>` lists
/// them. The ref is peeled to a commit ID first, so `git log` never reads the ref name as an
/// option, and the path comes after `--` as a literal pathspec.
pub(crate) async fn list_path_commits(
    repo_path: &Path,
    git_ref: &str,
    path: &str,
    offset: usize,
    limit: usize,
    filter: &CommitFilter,
) -> Result<Vec<CommitInfo>> {
    // No tree entry is named empty, `.` or `..`, so such a path changed in no commit. Git would
    // refuse it as a pathspec outside the repository instead.
    if limit == 0 || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Ok(Vec::new());
    }
    let tip = {
        let repo = Repository::open_bare(repo_path)?;
        let commit = peel_to_commit_checked(&repo, git_ref)?;
        commit.id().to_string()
    };
    let mut command = process::git_command()?;
    command
        .arg("--literal-pathspecs")
        .arg("log")
        .arg(format!("--skip={offset}"))
        .arg(format!("--max-count={limit}"))
        .arg("--format=%H");
    if let Some(author) = &filter.author {
        // The author text is a literal, matched without case, as `list_commits` matches it.
        command
            .arg("--fixed-strings")
            .arg("--regexp-ignore-case")
            .arg(format!("--author={author}"));
    }
    if let Some(since) = filter.since {
        command.arg(format!("--since-as-filter={since}"));
    }
    if let Some(until) = filter.until {
        command.arg(format!("--until={until}"));
    }
    command.arg(&tip).arg("--").arg(path).current_dir(repo_path);
    let (status, output) = process::buffered(
        command,
        b"",
        MAX_PATH_COMMIT_OUTPUT_BYTES,
        process::RPC_TIMEOUT,
    )
    .await?;
    ensure!(
        status.success(),
        "Git could not list the commits for the path"
    );
    let repo = Repository::open_bare(repo_path)?;
    let mut total_bytes = 0usize;
    std::str::from_utf8(&output)?
        .lines()
        .map(|line| -> Result<CommitInfo> {
            let commit = find_commit_checked(&repo, git2::Oid::from_str(line)?)?;
            add_commit_to_budget(
                &mut total_bytes,
                commit_object_size(&repo, commit.id())?,
                true,
            )?;
            Ok(commit_info(&commit))
        })
        .collect()
}

/// Each head's divergence from `base`, in the order of `heads`. Git counts both sides from their
/// merge base, so a branch with a long history of its own costs a longer walk.
pub(crate) fn branch_statuses(
    repo_path: &Path,
    base: &str,
    heads: &[String],
) -> Result<Vec<BranchStatus>> {
    let repo = Repository::open_bare(repo_path)?;
    let base = peel_to_commit_checked(&repo, base)?.id();
    heads
        .iter()
        .map(|head| {
            let tip = peel_to_commit_checked(&repo, head)?;
            let (ahead, behind) = repo.graph_ahead_behind(tip.id(), base)?;
            let updated = tip.committer().when().seconds();
            Ok(BranchStatus {
                ahead,
                behind,
                updated,
            })
        })
        .collect()
}

/// Decode an already opened commit without resolving a mutable repository path.
pub(crate) fn commit_info(commit: &git2::Commit<'_>) -> CommitInfo {
    let author = commit.author();
    let committer = commit.committer();
    let parents: Vec<String> = commit.parent_ids().map(|id| format!("{}", id)).collect();

    CommitInfo {
        sha: format!("{}", commit.id()),
        message: commit.message().unwrap_or("").to_string(),
        author_name: author.name().unwrap_or("").to_string(),
        author_email: author.email().unwrap_or("").to_string(),
        committer_name: committer.name().unwrap_or("").to_string(),
        committer_email: committer.email().unwrap_or("").to_string(),
        timestamp: author.when().seconds(),
        committer_timestamp: committer.when().seconds(),
        parents,
    }
}

pub(crate) async fn get_commit_diff(repo_path: &Path, sha: &str) -> Result<DiffInfo> {
    let (revision, root) = {
        let repo = Repository::open_bare(repo_path)?;
        let commit = peel_to_commit_checked(&repo, sha)?;
        let head = commit.id().to_string();
        let root = commit.parent_count() == 0;
        let revision = if root {
            head
        } else {
            format!("{}..{head}", commit.parent_id(0)?)
        };
        (revision, root)
    };
    diff_in_repository(repo_path, &revision, root, &[]).await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub sha: String,
    pub message: Option<String>,
    /// When the tag was made: the tagger's time for an annotated tag, else its commit's time.
    #[serde(default)]
    pub date: Option<i64>,
}

/// Check the same aggregate metadata budget as the tag reader before publishing replacements.
/// A receive may read its new objects from quarantine without exposing them to other readers.
pub(crate) fn check_tag_budget(
    repo_path: &Path,
    quarantine: Option<&Path>,
    replacements: &[(String, String)],
) -> Result<()> {
    let repo = Repository::open_bare(repo_path)?;
    let odb = repo.odb()?;
    if let Some(quarantine) = quarantine {
        odb.add_disk_alternate(
            quarantine
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid quarantine path"))?,
        )?;
    }
    let mut tags = std::collections::HashMap::new();
    repo.tag_foreach(|oid, name| {
        tags.insert(String::from_utf8_lossy(name).into_owned(), oid);
        tags.len() <= MAX_REPOSITORY_REFS
    })?;
    for (reference, new) in replacements {
        if new.bytes().all(|b| b == b'0') {
            tags.remove(reference);
        } else {
            tags.insert(reference.clone(), git2::Oid::from_str(new)?);
        }
    }
    if tags.len() > MAX_REPOSITORY_REFS {
        return Err(ObjectTooLarge.into());
    }
    let mut bytes = 0usize;
    for (name, oid) in tags {
        if name.len() > MAX_REF_NAME_BYTES {
            return Err(ObjectTooLarge.into());
        }
        let (size, kind) = odb.read_header(oid)?;
        if kind == ObjectType::Tag && size > MAX_TAG_OBJECT_BYTES {
            return Err(ObjectTooLarge.into());
        }
        bytes = bytes
            .checked_add(
                name.trim_start_matches("refs/tags/").len()
                    + 40
                    + if kind == ObjectType::Tag { size } else { 0 },
            )
            .filter(|total| *total <= MAX_TAG_RESPONSE_BYTES)
            .ok_or(ObjectTooLarge)?;
    }
    Ok(())
}

pub(crate) fn list_tags(repo_path: &Path) -> Result<Vec<TagInfo>> {
    let repo = Repository::open_bare(repo_path)?;
    let odb = repo.odb()?;
    let mut tags = Vec::new();
    let mut total_bytes = 0usize;
    let mut message_bytes = 0usize;
    let mut failure = None;

    repo.tag_foreach(|oid, name| {
        let result = (|| {
            if tags.len() >= MAX_REPOSITORY_REFS || name.len() > MAX_REF_NAME_BYTES {
                return Err(ObjectTooLarge.into());
            }
            let (object_size, kind) = odb.read_header(oid)?;
            if kind == ObjectType::Tag && object_size > MAX_TAG_OBJECT_BYTES {
                return Err(ObjectTooLarge.into());
            }
            let name = String::from_utf8_lossy(name);
            let name = name.strip_prefix("refs/tags/").unwrap_or(&name).to_owned();
            let (message, date) = if kind == ObjectType::Tag {
                let tag = repo.find_tag(oid)?;
                let date = tag.tagger().map(|tagger| tagger.when().seconds());
                let message = tag.message().filter(|message| {
                    if message.len() <= MAX_TAG_RESPONSE_BYTES - message_bytes {
                        message_bytes += message.len();
                        true
                    } else {
                        false
                    }
                });
                (message.map(ToOwned::to_owned), date)
            } else {
                let date = find_commit_checked(&repo, oid)
                    .ok()
                    .map(|commit| commit.time().seconds());
                (None, date)
            };
            let bytes = name.len() + 40;
            total_bytes = total_bytes
                .checked_add(bytes)
                .filter(|total| *total <= MAX_TAG_RESPONSE_BYTES)
                .ok_or(ObjectTooLarge)?;
            tags.push(TagInfo {
                name,
                sha: oid.to_string(),
                message,
                date,
            });
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = result {
            failure = Some(error);
            false
        } else {
            true
        }
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(tags)
}

/// Resolve a tag target and, for an annotated tag, write only the tag object.
/// The caller publishes the ref after its live authority check.
pub(crate) fn prepare_create_tag(
    repo_path: &Path,
    tag_name: &str,
    target_sha: &str,
    message: Option<&str>,
    tagger_name: &str,
    tagger_email: &str,
) -> Result<TagInfo> {
    let repo = Repository::open_bare(repo_path)?;
    let reference =
        crate::backend::git::tag_ref(tag_name).ok_or_else(|| anyhow!("Invalid tag name"))?;
    if repo.find_reference(&reference).is_ok() {
        return Err(anyhow!("Tag '{tag_name}' already exists"));
    }
    let oid =
        git2::Oid::from_str(target_sha).map_err(|_| anyhow!("Invalid commit SHA: {target_sha}"))?;
    let commit =
        find_commit_checked(&repo, oid).map_err(|_| anyhow!("Commit '{target_sha}' not found"))?;
    let (target, date) = if let Some(message) = message {
        let signature = git2::Signature::now(tagger_name, tagger_email)?;
        let target =
            repo.tag_annotation_create(tag_name, commit.as_object(), &signature, message)?;
        (target, signature.when().seconds())
    } else {
        (commit.id(), commit.time().seconds())
    };
    Ok(TagInfo {
        name: tag_name.to_owned(),
        sha: target.to_string(),
        message: message.map(str::to_owned),
        date: Some(date),
    })
}

pub(crate) fn prepare_delete_tag(repo_path: &Path, tag_name: &str) -> Result<String> {
    let repo = Repository::open_bare(repo_path)?;
    let reference =
        crate::backend::git::tag_ref(tag_name).ok_or_else(|| anyhow!("Invalid tag name"))?;
    let old = repo
        .find_reference(&reference)?
        .target()
        .map(|oid| oid.to_string())
        .ok_or_else(|| anyhow!("Tag '{tag_name}' is not a direct reference"))?;
    Ok(old)
}

// Merging by branch name alone could merge a newer head than the one reviewed
// or discard base history. Callers use prepare_merge and publish_merge with
// explicit commit IDs; publish_merge fails if either branch has moved since.

pub const MAX_DIFF_FILES: usize = 1_000;
pub const MAX_DIFF_FILE_PATCH_BYTES: usize = 256 * 1024;
pub const MAX_DIFF_TOTAL_PATCH_BYTES: usize = 1024 * 1024;
const MAX_DIFF_PATH_BYTES: usize = 4 * 1024;
const MAX_DIFF_PATH_TOTAL_BYTES: usize = 256 * 1024;
const MAX_DIFF_METADATA_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffInfo {
    pub files: Vec<DiffFileInfo>,
    pub additions: i32,
    pub deletions: i32,
    pub changed_files: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffFileInfo {
    pub filename: String,
    pub status: String,
    pub additions: i32,
    pub deletions: i32,
    pub patch: String,
    /// The blob id of the file's new side. A reader's viewed mark is kept against it.
    #[serde(default)]
    pub blob_sha: String,
}

pub(crate) fn prepare_create_branch(
    repo_path: &Path,
    branch_name: &str,
    source_sha: &str,
) -> Result<String> {
    let repo = Repository::open_bare(repo_path)?;
    let reference = crate::backend::git::branch_ref(branch_name)
        .ok_or_else(|| anyhow!("Invalid branch name"))?;
    if repo.find_reference(&reference).is_ok() {
        return Err(anyhow!("Branch '{branch_name}' already exists"));
    }
    let oid =
        git2::Oid::from_str(source_sha).map_err(|_| anyhow!("Invalid commit SHA: {source_sha}"))?;
    find_commit_checked(&repo, oid)
        .map(|commit| commit.id().to_string())
        .map_err(|_| anyhow!("Commit '{source_sha}' not found"))
}

pub(crate) fn prepare_delete_branch(repo_path: &Path, branch_name: &str) -> Result<String> {
    let repo = Repository::open_bare(repo_path)?;
    let reference = crate::backend::git::branch_ref(branch_name)
        .ok_or_else(|| anyhow!("Invalid branch name"))?;
    let branch = repo
        .find_branch(branch_name, git2::BranchType::Local)
        .map_err(|_| anyhow!("Branch '{branch_name}' not found"))?;
    if branch.is_head() {
        return Err(anyhow!("Cannot delete the default branch '{branch_name}'"));
    }
    branch
        .get()
        .target()
        .map(|oid| oid.to_string())
        .ok_or_else(|| anyhow!("Branch '{reference}' is not a direct reference"))
}

/// Compares repositories without importing either one's objects into the other.
pub(crate) async fn compare_repositories(
    base_path: &Path,
    head_path: &Path,
    base_ref: &str,
    head_ref: &str,
) -> Result<DiffInfo> {
    let base_repo = Repository::open_bare(base_path)?;
    let head_repo = Repository::open_bare(head_path)?;
    let base = peel_to_commit_checked(&base_repo, base_ref)?.id();
    let head = peel_to_commit_checked(&head_repo, head_ref)?.id();
    let temporary = tempfile::tempdir()?;
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).external_template(false);
    let repo = Repository::init_opts(temporary.path(), &options)?;
    let odb = repo.odb()?;
    let mut alternates = String::new();
    for source in [&base_repo, &head_repo] {
        let objects = source.path().join("objects").canonicalize()?;
        let path = objects
            .to_str()
            .ok_or_else(|| anyhow!("Invalid object storage path"))?;
        // libgit2 borrows through this handle; subprocess Git reads the same paths below.
        odb.add_disk_alternate(path)?;
        alternates.push('"');
        for character in path.chars() {
            match character {
                '\\' | '"' => {
                    alternates.push('\\');
                    alternates.push(character);
                }
                control if control.is_ascii_control() => {
                    alternates.push_str(&format!("\\{:03o}", control as u8));
                }
                other => alternates.push(other),
            }
        }
        alternates.push_str("\"\n");
    }
    std::fs::write(repo.path().join("objects/info/alternates"), alternates)?;
    repo.set_head_detached(base)?;
    let path = repo.path().to_owned();
    drop(odb);
    drop(repo);
    drop(base_repo);
    drop(head_repo);
    diff_in_repository(&path, &format!("{base}...{head}"), false, &[]).await
}

pub(crate) async fn get_diff(repo_path: &Path, base_ref: &str, head_ref: &str) -> Result<DiffInfo> {
    let repo = Repository::open_bare(repo_path)?;
    let base = peel_to_commit_checked(&repo, base_ref)?.id();
    let head = peel_to_commit_checked(&repo, head_ref)?.id();
    drop(repo);
    diff_in_repository(repo_path, &format!("{base}...{head}"), false, &[]).await
}

/// The diff of one file in a three-dot comparison, as the whole comparison displays it. The file is
/// found in the comparison's metadata first. Its patch is then read with both of its paths, so Git
/// still pairs a rename's two sides and the patch matches the displayed one. An oversized patch in
/// another file does not stop it, and a file the comparison does not show gives an empty diff.
pub(crate) async fn get_file_diff(
    repo_path: &Path,
    base_ref: &str,
    head_ref: &str,
    path: &str,
) -> Result<DiffInfo> {
    let repo = Repository::open_bare(repo_path)?;
    let base = peel_to_commit_checked(&repo, base_ref)?.id();
    let head = peel_to_commit_checked(&repo, head_ref)?.id();
    drop(repo);
    let revision = format!("{base}...{head}");
    let metadata = diff_metadata(repo_path, &revision, false, &[]).await?;
    let Some(file) = metadata.iter().find(|file| file.filename == path) else {
        return Ok(DiffInfo {
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            changed_files: 0,
        });
    };
    let mut paths: Vec<&[u8]> = [file.old_path.as_slice(), file.new_path.as_slice()]
        .into_iter()
        .filter(|path| !path.is_empty())
        .collect();
    paths.dedup();
    let pathspecs = literal_pathspecs(&paths);
    diff_in_repository(repo_path, &revision, false, &pathspecs).await
}

/// Literal pathspecs: the `:(literal)` magic makes every character of a path an ordinary one.
fn literal_pathspecs(paths: &[&[u8]]) -> Vec<OsString> {
    paths
        .iter()
        .map(|path| {
            let mut spec = b":(literal)".to_vec();
            spec.extend_from_slice(path);
            OsString::from_vec(spec)
        })
        .collect()
}

pub const MAX_CI_CHANGED_PATHS: usize = 8_192;
pub const MAX_CI_CHANGED_PATH_BYTES: usize = 4_096;
pub const MAX_CI_CHANGED_PATH_TOTAL_BYTES: usize = 1024 * 1024;

/// Returns only path names for a three-dot comparison (what a pull request adds to its
/// base), or for a comparison of the two trees themselves when `endpoints` is set (what a
/// push changed, including a rewind). Every path is returned when the base is the
/// all-zero unborn revision. Output is bounded while Git is running; patch bodies are
/// never requested or materialized.
pub(crate) async fn changed_paths(
    repo_path: &Path,
    base_ref: &str,
    head_ref: &str,
    endpoints: bool,
) -> Result<Vec<String>> {
    let repo = Repository::open_bare(repo_path)?;
    let unborn = base_ref.len() == 40 && base_ref.bytes().all(|byte| byte == b'0');
    let base = if unborn {
        let tree = repo.treebuilder(None)?;
        tree.write()?
    } else {
        peel_to_commit_checked(&repo, base_ref)?.id()
    };
    let head = peel_to_commit_checked(&repo, head_ref)?.id();
    let mut command = process::git_command()?;
    command.args([
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--name-only",
        "-z",
    ]);
    if unborn || endpoints {
        command.arg(base.to_string()).arg(head.to_string());
    } else {
        command.arg(format!("{base}...{head}"));
    }
    command.arg("--").current_dir(repo_path);
    let output_limit = MAX_CI_CHANGED_PATH_TOTAL_BYTES + MAX_CI_CHANGED_PATHS;
    let (status, output) =
        process::buffered(command, b"", output_limit, process::RPC_TIMEOUT).await?;
    anyhow::ensure!(status.success(), "Git changed-path comparison failed");

    let mut total = 0usize;
    let mut paths = Vec::new();
    for bytes in output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        anyhow::ensure!(paths.len() < MAX_CI_CHANGED_PATHS, "too many changed paths");
        anyhow::ensure!(
            bytes.len() <= MAX_CI_CHANGED_PATH_BYTES,
            "changed path is too long"
        );
        total = total
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_CI_CHANGED_PATH_TOTAL_BYTES)
            .ok_or_else(|| anyhow!("changed paths exceed their total size limit"))?;
        paths.push(std::str::from_utf8(bytes)?.to_owned());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[derive(Debug)]
struct DiffFileMetadata {
    old_path: Vec<u8>,
    new_path: Vec<u8>,
    filename: String,
    status: &'static str,
    blob_sha: String,
    additions: i32,
    deletions: i32,
}

fn nul_field<'a>(output: &'a [u8], cursor: &mut usize) -> Result<&'a [u8]> {
    let rest = output
        .get(*cursor..)
        .ok_or_else(|| anyhow!("Malformed git diff output"))?;
    let length = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| anyhow!("Malformed git diff output"))?;
    *cursor += length + 1;
    Ok(&rest[..length])
}

fn diff_count(value: &[u8]) -> Result<i32> {
    if value == b"-" {
        return Ok(0);
    }
    let value = std::str::from_utf8(value)?.parse::<u64>()?;
    i32::try_from(value).map_err(|_| ObjectTooLarge.into())
}

fn parse_diff_metadata(output: &[u8]) -> Result<Vec<DiffFileMetadata>> {
    if output.len() > MAX_DIFF_METADATA_BYTES {
        return Err(ObjectTooLarge.into());
    }
    let mut cursor = 0usize;
    let mut path_bytes = 0usize;
    let mut files = Vec::new();
    while output.get(cursor) == Some(&b':') {
        if files.len() >= MAX_DIFF_FILES {
            return Err(ObjectTooLarge.into());
        }
        let header = nul_field(output, &mut cursor)?;
        let status = header
            .split(|byte| *byte == b' ')
            .next_back()
            .and_then(|status| status.first())
            .copied()
            .ok_or_else(|| anyhow!("Malformed git diff status"))?;
        // The raw header is `:old-mode new-mode old-blob new-blob status`.
        let blob_sha = header
            .split(|byte| *byte == b' ')
            .nth(3)
            .ok_or_else(|| anyhow!("Malformed git diff blob"))?;
        let blob_sha = String::from_utf8_lossy(blob_sha).into_owned();
        let old_path = nul_field(output, &mut cursor)?;
        let new_path = if matches!(status, b'R' | b'C') {
            nul_field(output, &mut cursor)?
        } else {
            old_path
        };
        if old_path.len() > MAX_DIFF_PATH_BYTES || new_path.len() > MAX_DIFF_PATH_BYTES {
            return Err(ObjectTooLarge.into());
        }
        path_bytes = path_bytes
            .checked_add(old_path.len())
            .filter(|total| *total <= MAX_DIFF_PATH_TOTAL_BYTES)
            .ok_or(ObjectTooLarge)?;
        if matches!(status, b'R' | b'C') {
            path_bytes = path_bytes
                .checked_add(new_path.len())
                .filter(|total| *total <= MAX_DIFF_PATH_TOTAL_BYTES)
                .ok_or(ObjectTooLarge)?;
        }
        let (filename, status_name) = match status {
            b'A' => (new_path, "added"),
            b'D' => (old_path, "removed"),
            b'R' => (new_path, "renamed"),
            b'C' => (new_path, "added"),
            b'T' => (new_path, "type_changed"),
            _ => (new_path, "modified"),
        };
        files.push(DiffFileMetadata {
            old_path: old_path.to_vec(),
            new_path: new_path.to_vec(),
            filename: String::from_utf8_lossy(filename).into_owned(),
            status: status_name,
            blob_sha,
            additions: 0,
            deletions: 0,
        });
    }

    for file in &mut files {
        let record = nul_field(output, &mut cursor)?;
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        file.additions = diff_count(
            fields
                .next()
                .ok_or_else(|| anyhow!("Missing diff additions"))?,
        )?;
        file.deletions = diff_count(
            fields
                .next()
                .ok_or_else(|| anyhow!("Missing diff deletions"))?,
        )?;
        let path = fields.next().ok_or_else(|| anyhow!("Missing diff path"))?;
        if path.is_empty() {
            let old_path = nul_field(output, &mut cursor)?;
            let new_path = nul_field(output, &mut cursor)?;
            ensure!(
                old_path == file.old_path.as_slice() && new_path == file.new_path.as_slice(),
                "Git diff records disagree"
            );
        } else {
            ensure!(
                path == file.old_path.as_slice() && path == file.new_path.as_slice(),
                "Git diff records disagree"
            );
        }
    }
    ensure!(cursor == output.len(), "Unexpected git diff records");
    Ok(files)
}

fn split_diff_patches(output: &[u8], files: &[DiffFileMetadata]) -> Result<Vec<String>> {
    if output.len() > MAX_DIFF_TOTAL_PATCH_BYTES {
        return Err(ObjectTooLarge.into());
    }
    if files.is_empty() {
        ensure!(output.is_empty(), "Unexpected git patch output");
        return Ok(Vec::new());
    }

    const MARKER: &[u8] = b"diff --git ";
    let mut starts = Vec::new();
    for index in 0..=output.len().saturating_sub(MARKER.len()) {
        if (index == 0 || output[index - 1] == b'\n')
            && output.get(index..index + MARKER.len()) == Some(MARKER)
        {
            starts.push(index);
            if starts.len() > 2 * MAX_DIFF_FILES {
                return Err(ObjectTooLarge.into());
            }
        }
    }
    let expected: usize = files
        .iter()
        .map(|file| if file.status == "type_changed" { 2 } else { 1 })
        .sum();
    ensure!(starts.len() == expected, "Git patch records disagree");
    let mut patches = Vec::with_capacity(files.len());
    let mut position = 0;
    for file in files {
        let start = starts[position];
        // Git emits a deletion and an addition for one raw type-change record.
        position += if file.status == "type_changed" { 2 } else { 1 };
        let end = starts.get(position).copied().unwrap_or(output.len());
        let patch = &output[start..end];
        if patch.len() > MAX_DIFF_FILE_PATCH_BYTES {
            return Err(ObjectTooLarge.into());
        }
        patches.push(String::from_utf8_lossy(patch).into_owned());
    }
    Ok(patches)
}

/// The comparison's per-file metadata, without patches. `pathspecs` limits it when not empty.
async fn diff_metadata(
    repo_path: &Path,
    revision: &str,
    root: bool,
    pathspecs: &[OsString],
) -> Result<Vec<DiffFileMetadata>> {
    let mut metadata = process::git_command()?;
    if root {
        metadata.args(["diff-tree", "--root", "--no-commit-id", "-r"]);
    } else {
        metadata.arg("diff");
    }
    metadata
        .args([
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--raw",
            "--abbrev=40",
            "--numstat",
            "-z",
            revision,
            "--",
        ])
        .args(pathspecs)
        .current_dir(repo_path);
    let (status, output) =
        process::buffered(metadata, b"", MAX_DIFF_METADATA_BYTES, process::RPC_TIMEOUT).await?;
    ensure!(status.success(), "Failed to get diff metadata");
    parse_diff_metadata(&output)
}

/// The diff of a comparison, limited to `pathspecs` when not empty.
async fn diff_in_repository(
    repo_path: &Path,
    revision: &str,
    root: bool,
    pathspecs: &[OsString],
) -> Result<DiffInfo> {
    let metadata = diff_metadata(repo_path, revision, root, pathspecs).await?;

    let patches = if metadata.is_empty() {
        Vec::new()
    } else {
        let mut command = process::git_command()?;
        if root {
            command.args(["diff-tree", "--root", "--no-commit-id", "-r"]);
        } else {
            command.arg("diff");
        }
        command
            .args([
                "--no-ext-diff",
                "--no-textconv",
                "--find-renames",
                "--patch",
                revision,
                "--",
            ])
            .args(pathspecs)
            .current_dir(repo_path);
        let (status, output) = process::buffered(
            command,
            b"",
            MAX_DIFF_TOTAL_PATCH_BYTES,
            process::RPC_TIMEOUT,
        )
        .await?;
        ensure!(status.success(), "Failed to get diff patch");
        split_diff_patches(&output, &metadata)?
    };

    let mut additions = 0i32;
    let mut deletions = 0i32;
    let mut files = Vec::with_capacity(metadata.len());
    for (metadata, patch) in metadata.into_iter().zip(patches) {
        additions = additions
            .checked_add(metadata.additions)
            .ok_or(ObjectTooLarge)?;
        deletions = deletions
            .checked_add(metadata.deletions)
            .ok_or(ObjectTooLarge)?;
        files.push(DiffFileInfo {
            filename: metadata.filename,
            status: metadata.status.to_owned(),
            additions: metadata.additions,
            deletions: metadata.deletions,
            patch,
            blob_sha: metadata.blob_sha,
        });
    }
    Ok(DiffInfo {
        changed_files: files.len() as i32,
        files,
        additions,
        deletions,
    })
}

#[cfg(test)]
mod regression_tests;

#[cfg(test)]
mod read_limit_tests {
    use super::*;
    use crate::backend::git::ci::{
        list_ci_workflow_files, load_push_workflows, read_ci_workflow, resolve_ci_commit,
    };

    fn metadata_for_files(count: usize) -> Vec<u8> {
        let mut output = Vec::new();
        for index in 0..count {
            output.extend_from_slice(b":000000 100644 0000000 1111111 A\0");
            output.extend_from_slice(format!("file-{index}").as_bytes());
            output.push(0);
        }
        for index in 0..count {
            output.extend_from_slice(b"1\t0\t");
            output.extend_from_slice(format!("file-{index}").as_bytes());
            output.push(0);
        }
        output
    }

    #[test]
    fn diff_metadata_parses_add_delete_modify_and_rename_paths() {
        let mut output = Vec::new();
        for record in [
            b":000000 100644 0000000 1111111 A\0added.txt\0".as_slice(),
            b":100644 000000 1111111 0000000 D\0deleted.txt\0".as_slice(),
            b":100644 100644 1111111 2222222 M\0modified.txt\0".as_slice(),
            b":100644 100644 1111111 1111111 R100\0old.txt\0new.txt\0".as_slice(),
        ] {
            output.extend_from_slice(record);
        }
        for record in [
            b"1\t0\tadded.txt\0".as_slice(),
            b"0\t1\tdeleted.txt\0".as_slice(),
            b"1\t1\tmodified.txt\0".as_slice(),
            b"0\t0\t\0old.txt\0new.txt\0".as_slice(),
        ] {
            output.extend_from_slice(record);
        }
        let files = parse_diff_metadata(&output).unwrap();
        let values: Vec<_> = files
            .iter()
            .map(|file| {
                (
                    file.filename.as_str(),
                    file.status,
                    file.additions,
                    file.deletions,
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                ("added.txt", "added", 1, 0),
                ("deleted.txt", "removed", 0, 1),
                ("modified.txt", "modified", 1, 1),
                ("new.txt", "renamed", 0, 0),
            ]
        );
    }

    #[test]
    fn diff_file_limit_accepts_exactly_the_limit_and_rejects_one_more() {
        assert_eq!(
            parse_diff_metadata(&metadata_for_files(MAX_DIFF_FILES))
                .unwrap()
                .len(),
            MAX_DIFF_FILES
        );
        let error = parse_diff_metadata(&metadata_for_files(MAX_DIFF_FILES + 1)).unwrap_err();
        assert!(error.is::<ObjectTooLarge>());
    }

    fn patch(length: usize) -> Vec<u8> {
        let mut patch = vec![b'x'; length];
        patch[..b"diff --git a/a b/a\n".len()].copy_from_slice(b"diff --git a/a b/a\n");
        *patch.last_mut().unwrap() = b'\n';
        patch
    }

    #[test]
    fn patch_limits_accept_exact_values_and_reject_one_over() {
        let exact_file = patch(MAX_DIFF_FILE_PATCH_BYTES);
        let one_file = parse_diff_metadata(&metadata_for_files(1)).unwrap();
        assert_eq!(split_diff_patches(&exact_file, &one_file).unwrap().len(), 1);
        let error =
            split_diff_patches(&patch(MAX_DIFF_FILE_PATCH_BYTES + 1), &one_file).unwrap_err();
        assert!(error.is::<ObjectTooLarge>());

        let mut exact_total = Vec::new();
        for _ in 0..(MAX_DIFF_TOTAL_PATCH_BYTES / MAX_DIFF_FILE_PATCH_BYTES) {
            exact_total.extend_from_slice(&exact_file);
        }
        let files = parse_diff_metadata(&metadata_for_files(
            MAX_DIFF_TOTAL_PATCH_BYTES / MAX_DIFF_FILE_PATCH_BYTES,
        ))
        .unwrap();
        assert_eq!(
            split_diff_patches(&exact_total, &files).unwrap().len(),
            files.len()
        );
        exact_total.push(b'x');
        let error = split_diff_patches(&exact_total, &files).unwrap_err();
        assert!(error.is::<ObjectTooLarge>());
    }

    #[test]
    fn type_change_patch_limit_covers_both_sections() {
        let mut files = parse_diff_metadata(&metadata_for_files(1)).unwrap();
        files[0].status = "type_changed";
        let mut output = patch(MAX_DIFF_FILE_PATCH_BYTES / 2);
        output.extend_from_slice(&patch(MAX_DIFF_FILE_PATCH_BYTES / 2));
        let patches = split_diff_patches(&output, &files).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].len(), MAX_DIFF_FILE_PATCH_BYTES);
        output.push(b'x');
        assert!(split_diff_patches(&output, &files)
            .unwrap_err()
            .is::<ObjectTooLarge>());
    }

    #[test]
    fn commit_metadata_budgets_accept_exactly_the_limit_and_reject_one_more() {
        let mut total = 0;
        assert!(add_commit_to_budget(&mut total, MAX_COMMIT_OBJECT_BYTES, false).is_ok());
        assert!(
            add_commit_to_budget(&mut total, MAX_COMMIT_OBJECT_BYTES + 1, false)
                .unwrap_err()
                .is::<ObjectTooLarge>()
        );

        let mut total = MAX_COMMIT_PAGE_BYTES - MAX_COMMIT_OBJECT_BYTES;
        assert!(add_commit_to_budget(&mut total, MAX_COMMIT_OBJECT_BYTES, true).is_ok());
        assert_eq!(total, MAX_COMMIT_PAGE_BYTES);
        assert!(add_commit_to_budget(&mut total, 1, true)
            .unwrap_err()
            .is::<ObjectTooLarge>());
    }

    #[test]
    fn tree_and_repository_info_counts_are_inclusive_at_the_limit() {
        assert!(check_tree_materialization(MAX_TREE_ENTRIES, 0).is_ok());
        assert!(check_tree_materialization(MAX_TREE_ENTRIES + 1, 0)
            .unwrap_err()
            .is::<ObjectTooLarge>());
        let exact = vec![b'a'; MAX_READ_REFS]
            .into_iter()
            .flat_map(|byte| [byte, b'\n'])
            .collect::<Vec<_>>();
        assert_eq!(parse_branches(&exact).unwrap().len(), MAX_READ_REFS);
        let mut over = exact;
        over.extend_from_slice(b"a\n");
        assert!(parse_branches(&over).unwrap_err().is::<ObjectTooLarge>());
    }

    #[test]
    fn replicas_list_more_refs_than_pages_do() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let commit = repo
            .commit(None, &signature, &signature, "empty", &tree, &[])
            .unwrap();
        for index in 0..=MAX_READ_REFS {
            repo.reference(&format!("refs/tags/t{index}"), commit, false, "")
                .unwrap();
        }
        repo.reference("refs/pull/1/head", commit, false, "")
            .unwrap();

        assert_eq!(
            branches_and_tags_in_use(directory.path()).unwrap().count,
            MAX_READ_REFS + 1
        );
        assert!(list_refs(directory.path())
            .unwrap_err()
            .is::<ObjectTooLarge>());
        let mirrored = list_mirror_refs(directory.path()).unwrap();
        assert_eq!(mirrored.len(), MAX_READ_REFS + 2);
    }

    /// A rewind leaves the new tip as the merge base, so the pull-request comparison is
    /// empty while the push really did change the file.
    #[tokio::test]
    async fn push_comparison_sees_a_rewind_and_pull_request_comparison_does_not() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let mut commits = Vec::new();
        for content in ["one", "two"] {
            let blob = repo.blob(content.as_bytes()).unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert("a.txt", blob, 0o100644).unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let parents: Vec<_> = commits
                .last()
                .map(|id| repo.find_commit(*id).unwrap())
                .into_iter()
                .collect();
            let parents: Vec<_> = parents.iter().collect();
            commits.push(
                repo.commit(None, &signature, &signature, content, &tree, &parents)
                    .unwrap(),
            );
        }
        let (first, second) = (commits[0].to_string(), commits[1].to_string());
        assert_eq!(
            changed_paths(directory.path(), &second, &first, true)
                .await
                .unwrap(),
            vec!["a.txt".to_owned()]
        );
        assert!(changed_paths(directory.path(), &second, &first, false)
            .await
            .unwrap()
            .is_empty());
    }

    /// A bare repository with a normal history on `main`, a commit over the size limit on
    /// `huge` and an annotated tag over that commit. Repositories copied from before the
    /// push limit may hold objects like these.
    fn repository_with_oversized_objects() -> (tempfile::TempDir, git2::Oid, git2::Oid) {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let blob = repo.blob(b"hello\n").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("README.md", blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let normal = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "normal",
                &tree,
                &[],
            )
            .unwrap();
        let parent = repo.find_commit(normal).unwrap();
        let huge = repo
            .commit(
                Some("refs/heads/huge"),
                &signature,
                &signature,
                &"x".repeat(MAX_COMMIT_OBJECT_BYTES + 1),
                &tree,
                &[&parent],
            )
            .unwrap();
        let huge_object = repo.find_object(huge, None).unwrap();
        repo.tag(
            "huge-tag",
            &huge_object,
            &signature,
            &"t".repeat(MAX_TAG_OBJECT_BYTES + 1),
            false,
        )
        .unwrap();
        (directory, normal, huge)
    }

    fn assert_too_large<T: std::fmt::Debug>(result: Result<T>) {
        let error = result.expect_err("an oversized object was read");
        assert!(error.is::<ObjectTooLarge>(), "unexpected error: {error:#}");
    }

    /// A revision expression is refused like a name that does not exist.
    fn assert_unknown_revision<T: std::fmt::Debug>(result: Result<T>) {
        let error = result.expect_err("a revision expression was accepted");
        let error = error
            .downcast_ref::<git2::Error>()
            .expect("a libgit2 error");
        assert_eq!(
            error.code(),
            git2::ErrorCode::NotFound,
            "unexpected error: {error}"
        );
    }

    /// Commits `tree` as the only commit on `reference`.
    fn commit_tree(repo: &Repository, reference: &str, tree: git2::Oid) -> git2::Oid {
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repo.find_tree(tree).unwrap();
        repo.commit(
            Some(reference),
            &signature,
            &signature,
            "fixture",
            &tree,
            &[],
        )
        .unwrap()
    }

    fn write_tree(repo: &Repository, entries: &[(&str, git2::Oid, i32)]) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, id, mode) in entries {
            builder.insert(*name, *id, *mode).unwrap();
        }
        builder.write().unwrap()
    }

    /// Writes a tree over the serialized size limit. Its long names all point at `blob`, so
    /// the tree is refused by size before any of its entries is read.
    fn write_oversized_tree(
        repo: &Repository,
        blob: git2::Oid,
        entries: &[(&str, git2::Oid, i32)],
    ) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for index in 0..=MAX_TREE_OBJECT_BYTES / 200 {
            builder
                .insert(format!("{index:04}-{}", "p".repeat(200)), blob, 0o100644)
                .unwrap();
        }
        for (name, id, mode) in entries {
            builder.insert(*name, *id, *mode).unwrap();
        }
        builder.write().unwrap()
    }

    /// A revision expression names an object, and libgit2 used to load that object before the
    /// commit check ran. Now the expression is an unknown revision, and names still resolve.
    #[test]
    fn revision_expressions_are_unknown_revisions() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let readme = repo.blob(b"hello\n").unwrap();
        let big = repo
            .blob(&vec![0u8; MAX_BLOB_MATERIALIZE_BYTES + 1])
            .unwrap();
        let entries = [("README.md", readme, 0o100644), ("big.bin", big, 0o100644)];
        let tree = write_tree(&repo, &entries);
        let normal = commit_tree(&repo, "refs/heads/main", tree).to_string();
        let path = directory.path();

        assert_eq!(get_commit(path, &normal).unwrap().sha, normal);
        assert_eq!(get_commit(path, &normal[..12]).unwrap().sha, normal);
        assert_eq!(get_commit(path, "main").unwrap().sha, normal);
        assert_eq!(
            blob_metadata(path, "main", "big.bin").unwrap().1,
            (MAX_BLOB_MATERIALIZE_BYTES + 1) as u64
        );

        assert_unknown_revision(get_commit(path, "main:big.bin"));
        assert_unknown_revision(get_commit(path, "main:README.md"));
    }

    /// The root tree is checked before any reader walks through it.
    #[tokio::test]
    async fn oversized_root_tree_refuses_path_reads() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let blob = repo.blob(b"hello\n").unwrap();
        let workflow = repo.blob(b"on: push\n").unwrap();
        let workflows = write_tree(&repo, &[("ci.yml", workflow, 0o100644)]);
        let github = write_tree(&repo, &[("workflows", workflows, 0o040000)]);
        let entries = [("small.txt", blob, 0o100644), (".github", github, 0o040000)];
        let root = write_oversized_tree(&repo, blob, &entries);
        let sha = commit_tree(&repo, "refs/heads/main", root).to_string();
        let path = directory.path();

        assert_too_large(list_tree(path, "main", "").await);
        assert_too_large(blob_metadata(path, "main", "small.txt"));
        assert_too_large(read_ci_workflow(path, &sha, ".github/workflows/ci.yml"));
        assert_too_large(list_ci_workflow_files(path, "main"));
    }

    /// An oversized directory refuses the paths under it, and the root is still readable.
    #[tokio::test]
    async fn oversized_intermediate_tree_refuses_paths_under_it() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let blob = repo.blob(b"hello\n").unwrap();
        let workflow = repo.blob(b"on: push\n").unwrap();
        let workflows = write_tree(&repo, &[("ci.yml", workflow, 0o100644)]);
        let github = write_oversized_tree(&repo, blob, &[("workflows", workflows, 0o040000)]);
        let docs = write_oversized_tree(&repo, blob, &[("small.txt", blob, 0o100644)]);
        let entries = [
            ("README.md", blob, 0o100644),
            ("docs", docs, 0o040000),
            (".github", github, 0o040000),
        ];
        let root = write_tree(&repo, &entries);
        let sha = commit_tree(&repo, "refs/heads/main", root).to_string();
        let path = directory.path();

        assert_eq!(blob_metadata(path, "main", "README.md").unwrap().1, 6);
        assert_too_large(list_tree(path, "main", "docs").await);
        assert_too_large(blob_metadata(path, "main", "docs/small.txt"));
        assert_too_large(read_ci_workflow(path, &sha, ".github/workflows/ci.yml"));
        assert_too_large(list_ci_workflow_files(path, "main"));
    }

    /// Paths through small directories read as before, and a missing name stays missing.
    #[tokio::test]
    async fn nested_path_reads_resolve_through_small_directories() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let blob = repo.blob(b"hello\n").unwrap();
        let workflow = repo.blob(b"on: push\n").unwrap();
        let workflows = write_tree(&repo, &[("ci.yml", workflow, 0o100644)]);
        let github = write_tree(&repo, &[("workflows", workflows, 0o040000)]);
        let docs = write_tree(&repo, &[("a.txt", blob, 0o100644)]);
        let root = write_tree(
            &repo,
            &[("docs", docs, 0o040000), (".github", github, 0o040000)],
        );
        let sha = commit_tree(&repo, "refs/heads/main", root).to_string();
        let plain = write_tree(&repo, &[("docs", docs, 0o040000)]);
        let plain_sha = commit_tree(&repo, "refs/heads/plain", plain).to_string();
        let path = directory.path();

        assert_eq!(blob_metadata(path, "main", "docs/a.txt").unwrap().1, 6);
        let file = read_ci_workflow(path, &sha, ".github/workflows/ci.yml").unwrap();
        assert_eq!(file.content, "on: push\n");
        assert_eq!(list_ci_workflow_files(path, "main").unwrap().len(), 1);
        assert_eq!(load_push_workflows(path, &sha).unwrap().1.len(), 1);
        assert_eq!(load_push_workflows(path, &plain_sha).unwrap().1, Vec::new());
        assert!(blob_metadata(path, "main", "docs/missing.txt").is_err());
        assert!(blob_metadata(path, "main", "docs/a.txt/inside").is_err());
        assert!(list_tree(path, "main", "docs/a.txt").await.is_err());
    }

    #[test]
    fn commit_reads_refuse_oversized_commits_and_tags_before_loading_them() {
        let (directory, normal, huge) = repository_with_oversized_objects();
        let path = directory.path();
        let (normal_sha, huge_sha) = (normal.to_string(), huge.to_string());

        // Normal commits in the same repository still read by full ID, short ID and name.
        assert_eq!(get_commit(path, &normal_sha).unwrap().sha, normal_sha);
        assert_eq!(get_commit(path, &normal_sha[..12]).unwrap().sha, normal_sha);
        assert_eq!(get_commit(path, "main").unwrap().sha, normal_sha);
        assert_eq!(
            list_commits(
                path,
                "main",
                0,
                10,
                &Default::default(),
                Instant::now() + COMMIT_WALK_TIMEOUT
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(blob_metadata(path, "main", "README.md").unwrap().1, 6);
        assert!(prepare_create_branch(path, "copy", &normal_sha).is_ok());
        assert!(resolve_ci_commit(path, "main", None, false).is_ok());
        // Ref listing reads only ref names, so the oversized branch and tag are listed.
        assert!(list_refs(path).unwrap().contains_key("refs/heads/huge"));

        assert_too_large(get_commit(path, &huge_sha));
        assert_too_large(get_commit(path, &huge_sha[..12]));
        assert_too_large(get_commit(path, "refs/heads/huge"));
        assert_too_large(get_commit(path, "huge-tag"));
        assert_unknown_revision(get_commit(path, "refs/heads/huge~1"));
        assert_unknown_revision(get_commit(path, "huge-tag^{commit}"));
        assert_unknown_revision(get_commit(path, "main^{commit}"));
        assert_too_large(list_commits(
            path,
            "refs/heads/huge",
            0,
            10,
            &Default::default(),
            Instant::now() + COMMIT_WALK_TIMEOUT,
        ));
        assert_too_large(list_tags(path));
        assert_too_large(blob_metadata(path, "refs/heads/huge", "README.md"));
        assert!(prepare_create_branch(path, "copy-huge", &huge_sha).is_err());
        assert_too_large(read_ci_workflow(
            path,
            &huge_sha,
            ".github/workflows/ci.yml",
        ));
        assert_too_large(list_ci_workflow_files(path, "refs/heads/huge"));
        assert_too_large(load_push_workflows(path, &huge_sha));
        assert_too_large(resolve_ci_commit(path, "main", Some(&huge_sha), false));
        assert!(resolve_ci_commit(path, "huge", None, false).is_err());
    }

    #[tokio::test]
    async fn tree_diff_and_archive_reads_refuse_oversized_commits() {
        let (directory, _, _) = repository_with_oversized_objects();
        let path = directory.path();

        let tree = list_tree(path, "main", "").await.unwrap();
        assert!(tree.iter().any(|entry| entry.name == "README.md"));
        // A file is not a directory, and is not read to find that out.
        let error = list_tree(path, "main", "README.md").await.unwrap_err();
        let error = error.downcast_ref::<git2::Error>().unwrap();
        assert_eq!(error.code(), git2::ErrorCode::NotFound);
        assert_eq!(error.class(), git2::ErrorClass::Tree);
        let revision = resolve_archive_revision(path, "main").unwrap();
        let zip = archive(path, revision, "demo", ArchiveFormat::Zip, 1024 * 1024)
            .await
            .unwrap();
        assert!(zip.starts_with(b"PK"));
        let targz = archive(path, revision, "demo", ArchiveFormat::TarGz, 1024 * 1024)
            .await
            .unwrap();
        assert!(targz.starts_with(&[0x1f, 0x8b]));

        assert_too_large(list_tree(path, "refs/heads/huge", "").await);
        assert_too_large(get_commit_diff(path, "huge-tag").await);
        assert_too_large(get_diff(path, "main", "refs/heads/huge").await);
        assert_too_large(changed_paths(path, "main", "refs/heads/huge", false).await);
        assert_too_large(compare_repositories(path, path, "main", "refs/heads/huge").await);
        assert_too_large(resolve_archive_revision(path, "refs/heads/huge"));
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[tokio::test]
    async fn repository_info_names_a_branch_that_shares_a_tag_name() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let commit = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "empty",
                &tree,
                &[],
            )
            .unwrap();
        repo.reference("refs/tags/main", commit, false, "").unwrap();
        repo.set_head("refs/heads/main").unwrap();

        let info = get_repo_info(directory.path()).await.unwrap();
        assert_eq!(info.default_branch, "main");
        assert_eq!(info.branches, ["main"]);
    }

    #[tokio::test]
    async fn commit_reads_and_tree_history_keep_author_and_committer_apart() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let author = git2::Signature::new(
            "Ada",
            "ada@example.invalid",
            &git2::Time::new(1_600_000_000, 0),
        )
        .unwrap();
        let committer = git2::Signature::new(
            "Grace",
            "grace@example.invalid",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let blob = repo.blob(b"hello\n").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("README.md", blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &author,
            &committer,
            "split identity",
            &tree,
            &[],
        )
        .unwrap();

        let commit = get_commit(directory.path(), "main").unwrap();
        assert_eq!(commit.author_name, "Ada");
        assert_eq!(commit.author_email, "ada@example.invalid");
        assert_eq!(commit.timestamp, 1_600_000_000);
        assert_eq!(commit.committer_name, "Grace");
        assert_eq!(commit.committer_email, "grace@example.invalid");
        assert_eq!(commit.committer_timestamp, 1_700_000_000);

        let metadata = list_tree_with_history(directory.path(), "main", "", false)
            .await
            .unwrap();
        assert!(metadata.iter().all(|entry| entry.last_commit.is_none()));
        assert_eq!(metadata[0].size, Some(6));
        let entries = list_tree(directory.path(), "main", "").await.unwrap();
        let last = entries[0].last_commit.as_ref().unwrap();
        assert_eq!(last.sha, commit.sha);
        assert_eq!(last.author_name, "Ada");
        assert_eq!(last.author_email, "ada@example.invalid");
        assert_eq!(last.timestamp, 1_600_000_000);
        assert_eq!(last.committer_name, "Grace");
        assert_eq!(last.committer_email, "grace@example.invalid");
        assert_eq!(last.committer_timestamp, 1_700_000_000);
    }
}

#[cfg(test)]
mod blame_tests {
    use super::*;

    /// Commits `content` as `docs/a.txt` on `main`, on top of `parent`.
    fn commit_file(
        repo: &Repository,
        parent: Option<git2::Oid>,
        author: &str,
        content: &[u8],
        message: &str,
    ) -> git2::Oid {
        let signature = git2::Signature::now(author, &format!("{author}@example.invalid")).unwrap();
        let mut docs = repo.treebuilder(None).unwrap();
        docs.insert("a.txt", repo.blob(content).unwrap(), 0o100644)
            .unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("docs", docs.write().unwrap(), 0o040000)
            .unwrap();
        let tree = repo.find_tree(root.write().unwrap()).unwrap();
        let parents: Vec<git2::Commit> = parent
            .map(|id| repo.find_commit(id).unwrap())
            .into_iter()
            .collect();
        let parents: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn blame_credits_each_run_of_lines_to_the_commit_that_changed_it() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let first = commit_file(&repo, None, "Alice", b"one\ntwo\nthree\n", "Add the file");
        let second = commit_file(
            &repo,
            Some(first),
            "Bob",
            b"one\nTWO\nthree\nfour\n",
            "Shout the second line\n\nA longer body.",
        );

        let blame = blame_file(directory.path(), "main", "docs/a.txt")
            .await
            .unwrap();
        assert_eq!(blame.content, "one\nTWO\nthree\nfour\n");
        assert_eq!(blame.size, blame.content.len() as u64);
        let runs: Vec<_> = blame
            .hunks
            .iter()
            .map(|hunk| {
                (
                    hunk.first_line,
                    hunk.line_count,
                    hunk.commit.clone(),
                    hunk.author_name.as_str(),
                    hunk.summary.as_str(),
                )
            })
            .collect();
        let (first, second) = (first.to_string(), second.to_string());
        assert_eq!(
            runs,
            [
                (1, 1, first.clone(), "Alice", "Add the file"),
                (2, 1, second.clone(), "Bob", "Shout the second line"),
                (3, 1, first, "Alice", "Add the file"),
                (4, 1, second, "Bob", "Shout the second line"),
            ]
        );
        assert!(blame.hunks.iter().all(|hunk| hunk.author_time > 0));
    }

    #[tokio::test]
    async fn blame_refuses_binary_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path()).unwrap();
        let binary = commit_file(&repo, None, "Alice", b"\0\x01binary", "Binary");
        let refusal = blame_file(directory.path(), "main", "docs/a.txt")
            .await
            .unwrap_err();
        assert_eq!(
            refusal.downcast_ref::<BlameRefused>().unwrap().0,
            "Binary files cannot be blamed."
        );

        let long_lines = "x\n".repeat(MAX_BLAME_LINES + 1);
        let lines = commit_file(&repo, Some(binary), "Alice", long_lines.as_bytes(), "Lines");
        let refusal = blame_file(directory.path(), "main", "docs/a.txt")
            .await
            .unwrap_err();
        assert_eq!(
            refusal.downcast_ref::<BlameRefused>().unwrap().0,
            "This file is too large to blame."
        );

        let heavy = vec![b'a'; MAX_BLAME_FILE_BYTES + 1];
        commit_file(&repo, Some(lines), "Alice", &heavy, "Heavy");
        let refusal = blame_file(directory.path(), "main", "docs/a.txt")
            .await
            .unwrap_err();
        assert!(refusal.downcast_ref::<BlameRefused>().is_some());
    }
}
