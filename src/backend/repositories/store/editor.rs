//! Single-file commits for the web editor and the file API: write the blob and
//! trees, create the commit, then move the branch only if it still points where the
//! editor started.
use super::{ErrorKind, RepoError};
use crate::backend::git::repository::{
    process, CommitInfo, MAX_COMMIT_EMAIL_BYTES, MAX_COMMIT_NAME_BYTES,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    path::Path,
    pin::Pin,
    process::ExitStatus,
    time::Duration,
};

/// The largest file `save_file` accepts.
pub const MAX_EDITOR_BYTES: usize = 1024 * 1024;
/// Limits of one upload commit: how many files, the largest file, and all the files together.
pub const MAX_UPLOAD_FILES: usize = 100;
pub const MAX_UPLOAD_FILE_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_UPLOAD_TOTAL_BYTES: usize = 100 * 1024 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);
pub(super) const ZERO: &str = "0000000000000000000000000000000000000000";

/// One file change to commit. The file's bytes are passed separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWrite {
    pub branch: String,
    pub path: String,
    pub message: String,
    /// A complete SHA, or `unborn` when the selected branch does not yet exist.
    pub expected_revision: String,
    pub creating: bool,
    /// Removes the file. The content passed with it is ignored.
    pub deleting: bool,
}

impl FileWrite {
    /// Checks everything that can be checked without looking at the repository.
    pub fn validate(&self) -> Result<(), RepoError> {
        validate_message(&self.message)?;
        validate_path(&self.path)?;
        validate_target(&self.branch, &self.expected_revision)?;
        if self.creating && self.deleting {
            return Err(invalid(
                "A file cannot be created and deleted in one commit.",
            ));
        }
        Ok(())
    }
}

/// One file of an upload: where it lands and how many bytes it has in the request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadFile {
    pub path: String,
    pub size: usize,
}

/// One commit that adds or replaces several files on a branch. The files' bytes follow the
/// request in the order of `files`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadWrite {
    pub branch: String,
    pub message: String,
    /// A complete SHA, or `unborn` when the selected branch does not yet exist.
    pub expected_revision: String,
    pub files: Vec<UploadFile>,
}

impl UploadWrite {
    /// Checks the commit, every path and the limits. The bytes are checked against the sizes
    /// when the commit is prepared.
    pub fn validate(&self) -> Result<(), RepoError> {
        validate_message(&self.message)?;
        validate_target(&self.branch, &self.expected_revision)?;
        if self.files.is_empty() || self.files.len() > MAX_UPLOAD_FILES {
            return Err(invalid("An upload needs 1 to 100 files."));
        }
        let mut paths = HashSet::new();
        let mut total = 0usize;
        for file in &self.files {
            validate_path(&file.path)?;
            if file.size > MAX_UPLOAD_FILE_BYTES {
                return Err(invalid("Each uploaded file can be at most 25 MiB."));
            }
            if !paths.insert(file.path.as_str()) {
                return Err(invalid("Each uploaded file needs its own path."));
            }
            total = total.saturating_add(file.size);
        }
        if total > MAX_UPLOAD_TOTAL_BYTES {
            return Err(invalid("An upload can be at most 100 MiB."));
        }
        Ok(())
    }
}

fn validate_message(message: &str) -> Result<(), RepoError> {
    if message.trim().is_empty() {
        return Err(invalid("A commit message is required."));
    }
    if message.len() > 4096 {
        return Err(invalid("A commit message can be at most 4096 bytes."));
    }
    if message.contains('\0') {
        return Err(invalid("A commit message cannot contain NUL bytes."));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), RepoError> {
    if path.is_empty() {
        return Err(invalid("A file path is required."));
    }
    if path.len() > 4096 {
        return Err(invalid("A file path can be at most 4096 bytes."));
    }
    if path.contains(['\\', '\0']) {
        return Err(invalid(
            "A file path cannot contain backslashes or NUL bytes.",
        ));
    }
    if path.split('/').count() > 64 {
        return Err(invalid("A file path can have at most 64 segments."));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == ".." || is_git_dir_name(part))
    {
        return Err(invalid(
            "Use a relative file path without empty, dot, parent or .git segments.",
        ));
    }
    Ok(())
}

/// `.git` in any case, with the trailing spaces and dots and the short name `git~1` that
/// Windows file systems treat as the same directory.
fn is_git_dir_name(part: &str) -> bool {
    let name = part.trim_end_matches([' ', '.']);
    name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case("git~1")
}

fn validate_target(branch: &str, expected_revision: &str) -> Result<(), RepoError> {
    if branch.is_empty() || crate::backend::git::branch_ref(branch).is_none() {
        return Err(invalid("Invalid branch name."));
    }
    if expected_revision != "unborn"
        && (expected_revision.len() != 40 || git2::Oid::from_str(expected_revision).is_err())
    {
        return Err(invalid("A complete expected branch revision is required."));
    }
    Ok(())
}

fn invalid(message: &'static str) -> RepoError {
    RepoError::new(ErrorKind::Invalid, message)
}
fn conflict() -> RepoError {
    RepoError::new(
        ErrorKind::Conflict,
        "The branch changed since the editor started.",
    )
}
fn unavailable() -> RepoError {
    RepoError::new(ErrorKind::Unavailable, "Repository storage is unavailable.")
}

/// A bare repository on disk that editor commands run against.
struct Repo<'a>(&'a Path);
impl Repo<'_> {
    async fn command(
        &self,
        args: &[&str],
        input: &[u8],
        identity: Option<(&str, &str)>,
    ) -> Result<(ExitStatus, Vec<u8>), RepoError> {
        let mut command = process::git_command().map_err(|_| unavailable())?;
        command.arg("--git-dir").arg(self.0).args(args);
        if let Some((name, email)) = identity {
            command
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_COMMITTER_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_COMMITTER_EMAIL", email);
        }
        process::buffered(command, input, MAX_OUTPUT, TIMEOUT)
            .await
            .map_err(|_| unavailable())
    }
    async fn checked(
        &self,
        args: &[&str],
        input: &[u8],
        identity: Option<(&str, &str)>,
    ) -> Result<Vec<u8>, RepoError> {
        let (status, bytes) = self.command(args, input, identity).await?;
        if !status.success() {
            return Err(unavailable());
        }
        Ok(bytes)
    }
    async fn direct_ref(&self, reference: &str) -> Result<Option<String>, RepoError> {
        let (symbolic, _) = self
            .command(&["symbolic-ref", "--quiet", reference], b"", None)
            .await?;
        if symbolic.code() != Some(1) {
            return Err(unavailable());
        }
        let (exists, _) = self
            .command(&["show-ref", "--exists", reference], b"", None)
            .await?;
        match exists.code() {
            Some(2) => Ok(None),
            Some(0) => Ok(Some(oid(&self
                .checked(&["show-ref", "--verify", "--hash", reference], b"", None)
                .await?)?)),
            _ => Err(unavailable()),
        }
    }
    /// Apply all changes under one directory, reading and writing each affected tree once.
    fn edit_tree<'a>(
        &'a self,
        tree: Option<String>,
        changes: &'a [Change<'a>],
        remaining: &'a mut usize,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, RepoError>> + Send + 'a>> {
        Box::pin(async move {
            let mut records = if let Some(tree) = tree {
                let bytes = self
                    .checked(&["ls-tree", "-z", "--full-tree", &tree], b"", None)
                    .await?;
                *remaining = remaining.checked_sub(bytes.len()).ok_or_else(unavailable)?;
                if !bytes.is_empty() && bytes.last() != Some(&0) {
                    return Err(unavailable());
                }
                bytes
                    .split(|b| *b == 0)
                    .filter(|b| !b.is_empty())
                    .try_fold(BTreeMap::new(), |mut records, record| {
                        let entry = split_entry(record)?;
                        if records
                            .insert(entry.name.to_vec(), record.to_vec())
                            .is_some()
                        {
                            return Err(unavailable());
                        }
                        Ok(records)
                    })?
            } else {
                BTreeMap::new()
            };
            let mut groups: BTreeMap<&str, Vec<Change<'_>>> = BTreeMap::new();
            for change in changes {
                let (name, tail) = change.path.split_once('/').unwrap_or((change.path, ""));
                groups.entry(name).or_default().push(Change {
                    path: tail,
                    content: change.content,
                    existing: change.existing,
                });
            }
            for (name, group) in groups {
                let record = records.remove(name.as_bytes());
                let existing = record.as_deref().map(split_entry).transpose()?;
                let new_entry = if group.iter().any(|change| change.path.is_empty()) {
                    if group.len() != 1 {
                        return Err(invalid("A parent path is not a directory."));
                    }
                    let change = &group[0];
                    let mode = match (&existing, change.existing) {
                        (Some(_), Existing::Forbid) => {
                            return Err(invalid("This path already exists. Use the file editor."))
                        }
                        (None, Existing::Require) => {
                            return Err(invalid("The file to edit does not exist."))
                        }
                        (Some(entry), _) => {
                            if entry.kind != "blob"
                                || (change.content.is_some()
                                    && !matches!(entry.mode, "100644" | "100755"))
                            {
                                return Err(invalid("Only regular files can be edited."));
                            }
                            entry.mode.to_owned()
                        }
                        (None, _) => "100644".to_owned(),
                    };
                    if let Some(content) = change.content {
                        let blob = oid(&self
                            .checked(&["hash-object", "-w", "--stdin"], content, None)
                            .await?)?;
                        Some(format!("{mode} blob {blob}\t{name}").into_bytes())
                    } else {
                        if existing.is_none() {
                            return Err(invalid("The file to delete does not exist."));
                        }
                        None
                    }
                } else {
                    let subtree = match &existing {
                        Some(entry) if entry.mode == "040000" && entry.kind == "tree" => {
                            Some(entry.object.to_owned())
                        }
                        Some(_) => return Err(invalid("A parent path is not a directory.")),
                        None => None,
                    };
                    self.edit_tree(subtree, &group, remaining)
                        .await?
                        .map(|subtree| format!("040000 tree {subtree}\t{name}").into_bytes())
                };
                if let Some(entry) = new_entry {
                    records.insert(name.as_bytes().to_vec(), entry);
                }
            }
            if records.is_empty() {
                return Ok(None);
            }
            let mut input = Vec::new();
            for record in records.into_values() {
                input.extend(record);
                input.push(0);
            }
            if input.len() > MAX_OUTPUT {
                return Err(unavailable());
            }
            Ok(Some(oid(&self
                .checked(&["mktree", "-z"], &input, None)
                .await?)?))
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedFile {
    pub branch: String,
    pub reference: String,
    pub expected: Option<String>,
    pub commit: CommitInfo,
}

/// One path a commit sets, and the bytes it gets. A change without bytes removes the file.
struct Change<'a> {
    path: &'a str,
    content: Option<&'a [u8]>,
    existing: Existing,
}

/// What a path may already hold: a new file must not replace one, an edit needs one, and an
/// upload may do either.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Existing {
    Forbid,
    Require,
    Either,
}

/// Builds one commit on `branch` that sets every change on top of the branch's tip. The branch
/// does not move here; `publish` moves it.
async fn prepare_commit(
    path: &Path,
    branch: &str,
    message: &str,
    expected_revision: &str,
    changes: &[Change<'_>],
    author: &str,
    email: &str,
) -> Result<PreparedFile, RepoError> {
    let storage = Repo(path);
    if changes.is_empty() {
        return Err(invalid("A commit needs at least one file."));
    }
    if author.is_empty()
        || author.len() > MAX_COMMIT_NAME_BYTES
        || email.is_empty()
        || email.len() > MAX_COMMIT_EMAIL_BYTES
        || author.contains(['\0', '\r', '\n', '<', '>'])
        || email.contains(['\0', '\r', '\n', '<', '>'])
    {
        return Err(invalid("Invalid commit identity."));
    }
    let reference =
        crate::backend::git::branch_ref(branch).ok_or_else(|| invalid("Invalid branch name."))?;
    let current = storage.direct_ref(&reference).await?;
    let expected = if expected_revision == "unborn" {
        None
    } else {
        Some(expected_revision.to_ascii_lowercase())
    };
    if current != expected {
        // The primary's own ref check tells a branch that moved from one that is gone.
        if current.is_none() {
            return Err(RepoError::branch_missing());
        }
        return Err(conflict());
    }
    let tree = match &current {
        Some(parent) => Some(oid(&storage
            .checked(
                &["rev-parse", "--verify", &format!("{parent}^{{tree}}")],
                b"",
                None,
            )
            .await?)?),
        None => None,
    };
    let mut budget = MAX_OUTPUT;
    let tree = storage.edit_tree(tree, changes, &mut budget).await?;
    // Deleting the last file leaves no tree at all, which the commit records as the empty tree.
    let new_tree = match tree {
        Some(tree) => tree,
        None => oid(&storage.checked(&["mktree", "-z"], b"", None).await?)?,
    };
    let mut args = vec!["commit-tree", new_tree.as_str()];
    if let Some(parent) = &current {
        args.extend(["-p", parent.as_str()]);
    }
    let sha = oid(&storage
        .checked(&args, message.as_bytes(), Some((author, email)))
        .await?)?;
    let bytes = storage
        .checked(&["cat-file", "commit", &sha], b"", None)
        .await?;
    let metadata = decode_commit(&bytes, &sha)?;
    if metadata.parents != current.iter().cloned().collect::<Vec<_>>() {
        return Err(unavailable());
    }
    Ok(PreparedFile {
        branch: branch.to_owned(),
        reference,
        expected,
        commit: metadata,
    })
}

/// Creates the new commit for one file without touching any ref.
pub(super) async fn prepare(
    path: &Path,
    input: &FileWrite,
    content: &[u8],
    author: &str,
    email: &str,
) -> Result<PreparedFile, RepoError> {
    input.validate()?;
    if content.len() > MAX_EDITOR_BYTES {
        return Err(invalid("File content exceeds 1 MiB."));
    }
    let change = Change {
        path: &input.path,
        content: (!input.deleting).then_some(content),
        existing: if input.creating {
            Existing::Forbid
        } else {
            Existing::Require
        },
    };
    prepare_commit(
        path,
        &input.branch,
        &input.message,
        &input.expected_revision,
        std::slice::from_ref(&change),
        author,
        email,
    )
    .await
}

/// Creates the one commit of an upload without touching any ref. Each file's bytes are the next
/// `size` bytes of `content`. A path that holds a file has that file replaced.
pub(super) async fn prepare_upload(
    path: &Path,
    write: &UploadWrite,
    content: &[u8],
    author: &str,
    email: &str,
) -> Result<PreparedFile, RepoError> {
    write.validate()?;
    let total: usize = write.files.iter().map(|file| file.size).sum();
    if total != content.len() {
        return Err(invalid("The uploaded bytes do not match the file list."));
    }
    let mut offset = 0;
    let mut changes = Vec::with_capacity(write.files.len());
    for file in &write.files {
        let end = offset + file.size;
        changes.push(Change {
            path: &file.path,
            content: Some(&content[offset..end]),
            existing: Existing::Either,
        });
        offset = end;
    }
    prepare_commit(
        path,
        &write.branch,
        &write.message,
        &write.expected_revision,
        &changes,
        author,
        email,
    )
    .await
}

/// Moves the branch to the prepared commit; `Conflict` if it moved meanwhile.
pub(super) async fn publish(path: &Path, prepared: &PreparedFile) -> Result<CommitInfo, RepoError> {
    let repo = Repo(path);
    if crate::backend::git::branch_ref(&prepared.branch).as_deref()
        != Some(prepared.reference.as_str())
        || !crate::backend::git::is_full_commit_id(&prepared.commit.sha)
        || prepared
            .expected
            .as_ref()
            .is_some_and(|expected| !crate::backend::git::is_full_commit_id(expected))
    {
        return Err(invalid("Invalid prepared file publication."));
    }
    let bytes = repo
        .checked(&["cat-file", "commit", &prepared.commit.sha], b"", None)
        .await?;
    let commit = decode_commit(&bytes, &prepared.commit.sha)?;
    if commit != prepared.commit
        || commit.parents != prepared.expected.iter().cloned().collect::<Vec<_>>()
    {
        return Err(invalid("Invalid prepared file publication."));
    }
    let transaction = format!(
        "start\noption no-deref\nupdate {} {} {}\nprepare\ncommit\n",
        prepared.reference,
        prepared.commit.sha,
        prepared.expected.as_deref().unwrap_or(ZERO)
    );
    let (status, _) = repo
        .command(&["update-ref", "--stdin"], transaction.as_bytes(), None)
        .await?;
    if !status.success() {
        if repo.direct_ref(&prepared.reference).await? != prepared.expected {
            return Err(conflict());
        }
        return Err(unavailable());
    }
    Ok(prepared.commit.clone())
}

fn oid(bytes: &[u8]) -> Result<String, RepoError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| unavailable())?
        .trim();
    if !crate::backend::git::is_full_commit_id(text) {
        return Err(unavailable());
    }
    Ok(text.to_ascii_lowercase())
}
struct TreeEntry<'a> {
    mode: &'a str,
    kind: &'a str,
    object: &'a str,
    name: &'a [u8],
}
fn split_entry(record: &[u8]) -> Result<TreeEntry<'_>, RepoError> {
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(unavailable)?;
    let header = std::str::from_utf8(&record[..tab]).map_err(|_| unavailable())?;
    let mut words = header.split(' ');
    let mode = words.next().ok_or_else(unavailable)?;
    let kind = words.next().ok_or_else(unavailable)?;
    let object = words.next().ok_or_else(unavailable)?;
    if words.next().is_some()
        || mode.len() != 6
        || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
        || !matches!(kind, "tree" | "blob" | "commit")
        || !crate::backend::git::is_full_commit_id(object)
        || record.len() == tab + 1
    {
        return Err(unavailable());
    }
    Ok(TreeEntry {
        mode,
        kind,
        object,
        name: &record[tab + 1..],
    })
}
fn decode_commit(bytes: &[u8], sha: &str) -> Result<CommitInfo, RepoError> {
    // The longest identities and a 4 KiB message make a commit of about 8.4 KB.
    if bytes.len() > 16 * 1024 {
        return Err(unavailable());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| unavailable())?;
    let (headers, message) = text.split_once("\n\n").ok_or_else(unavailable)?;
    let mut author = None;
    let mut committer = None;
    let mut parents = Vec::new();
    for header in headers.lines() {
        if let Some(value) = header.strip_prefix("author ") {
            author = Some(signature(value)?);
        } else if let Some(value) = header.strip_prefix("committer ") {
            committer = Some(signature(value)?);
        } else if let Some(value) = header.strip_prefix("parent ") {
            parents.push(oid(value.as_bytes())?);
        }
    }
    let (author_name, author_email, timestamp) = author.ok_or_else(unavailable)?;
    let (committer_name, committer_email, committer_timestamp) =
        committer.ok_or_else(unavailable)?;
    Ok(CommitInfo {
        sha: sha.into(),
        message: message.into(),
        author_name,
        author_email,
        committer_name,
        committer_email,
        timestamp,
        committer_timestamp,
        parents,
    })
}
fn signature(value: &str) -> Result<(String, String, i64), RepoError> {
    let (identity, time) = value.rsplit_once("> ").ok_or_else(unavailable)?;
    let (name, email) = identity.rsplit_once(" <").ok_or_else(unavailable)?;
    let (seconds, zone) = time.split_once(' ').ok_or_else(unavailable)?;
    if zone.len() != 5
        || !matches!(zone.as_bytes()[0], b'+' | b'-')
        || !zone.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(unavailable());
    }
    Ok((
        name.into(),
        email.into(),
        seconds.parse().map_err(|_| unavailable())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str) -> FileWrite {
        FileWrite {
            branch: "main".into(),
            path: path.into(),
            message: "save".into(),
            expected_revision: "unborn".into(),
            creating: true,
            deleting: false,
        }
    }

    fn blob_at(repo_path: &Path, git_ref: &str, path: &str) -> Vec<u8> {
        let repository = git2::Repository::open_bare(repo_path).unwrap();
        let tree = repository
            .revparse_single(git_ref)
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        let entry = tree.get_path(Path::new(path)).unwrap();
        let blob = repository.find_blob(entry.id()).unwrap();
        blob.content().to_vec()
    }

    /// Prepares and publishes in one go, like `save_file` without the lock.
    fn checked_write(path: &Path, input: &FileWrite, content: &[u8]) -> Result<String, RepoError> {
        checked_write_before_publish(path, input, content, || {})
    }

    fn checked_write_before_publish(
        path: &Path,
        input: &FileWrite,
        content: &[u8],
        before_publish: impl FnOnce(),
    ) -> Result<String, RepoError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let prepared =
                prepare(path, input, content, "tester", "tester@example.invalid").await?;
            before_publish();
            publish(path, &prepared).await.map(|commit| commit.sha)
        })
    }

    #[tokio::test]
    async fn maximum_upload_shares_deep_directory_trees_and_preserves_neighbors() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(root.path()).unwrap();
        let prefix = (0..63)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        let initial = input(&format!("{prefix}/keep"));
        let first = prepare(
            root.path(),
            &initial,
            b"keep",
            "Tester",
            "tester@example.invalid",
        )
        .await
        .unwrap();
        publish(root.path(), &first).await.unwrap();
        let write = UploadWrite {
            branch: "main".into(),
            message: "upload".into(),
            expected_revision: first.commit.sha,
            files: (0..MAX_UPLOAD_FILES)
                .map(|i| UploadFile {
                    path: format!("{prefix}/file-{i}"),
                    size: 1,
                })
                .collect(),
        };
        let bytes = vec![b'x'; MAX_UPLOAD_FILES];
        let prepared = tokio::time::timeout(
            Duration::from_secs(30),
            prepare_upload(
                root.path(),
                &write,
                &bytes,
                "Tester",
                "tester@example.invalid",
            ),
        )
        .await
        .expect("shared directories should not be rebuilt for every file")
        .unwrap();
        let tree = repo
            .find_commit(git2::Oid::from_str(&prepared.commit.sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        for file in &write.files {
            let entry = tree.get_path(Path::new(&file.path)).unwrap();
            assert_eq!(repo.find_blob(entry.id()).unwrap().content(), b"x");
        }
        let keep = tree.get_path(Path::new(&format!("{prefix}/keep"))).unwrap();
        assert_eq!(repo.find_blob(keep.id()).unwrap().content(), b"keep");
    }

    #[test]
    fn unsafe_paths_and_missing_revision_are_rejected() {
        for path in [
            "../x",
            "/x",
            "a//b",
            "a/.git/config",
            "a/../b",
            "a\\b",
            ".GIT/x",
            ".git /hooks/x",
            ".git./x",
            "git~1/x",
        ] {
            assert!(input(path).validate().is_err(), "{path}");
        }
        assert!(input(".github/x").validate().is_ok());
        assert!(input("git~2/x").validate().is_ok());
        let mut value = input("docs/file.md");
        value.expected_revision.clear();
        assert!(value.validate().is_err());
    }

    #[test]
    fn utf8_branch_is_valid_and_published_without_rewriting() {
        let temp = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(temp.path()).unwrap();
        let mut value = input("docs/file.md");
        value.branch = "équipe/功能".into();
        value.validate().unwrap();
        let published = checked_write(temp.path(), &value, b"utf8").unwrap();
        let repository = git2::Repository::open_bare(temp.path()).unwrap();
        assert_eq!(
            repository
                .find_reference("refs/heads/équipe/功能")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            published
        );
    }

    fn deletion(path: &str, expected: &str) -> FileWrite {
        FileWrite {
            branch: "main".into(),
            path: path.into(),
            message: "delete".into(),
            expected_revision: expected.into(),
            creating: false,
            deleting: true,
        }
    }

    #[test]
    fn deleting_removes_the_file_and_the_directories_it_empties() {
        let temp = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(temp.path()).unwrap();
        let first = checked_write(temp.path(), &input("docs/only.md"), b"one").unwrap();
        let mut keep = input("keep.txt");
        keep.expected_revision = first;
        let second = checked_write(temp.path(), &keep, b"two").unwrap();
        let entries = |sha: &str| -> Vec<String> {
            let repository = git2::Repository::open_bare(temp.path()).unwrap();
            let commit = repository
                .find_commit(git2::Oid::from_str(sha).unwrap())
                .unwrap();
            let tree = commit.tree().unwrap();
            tree.iter()
                .map(|entry| entry.name().unwrap().to_owned())
                .collect()
        };

        // The directory that held only the deleted file goes with it.
        let third = checked_write(temp.path(), &deletion("docs/only.md", &second), b"").unwrap();
        assert_eq!(entries(&third), vec!["keep.txt"]);
        // A branch that moved on refuses the delete and keeps the file.
        let stale = checked_write(temp.path(), &deletion("keep.txt", &second), b"").unwrap_err();
        assert_eq!(stale.kind, ErrorKind::Conflict);
        // Removing the last file leaves the empty tree, and a file that is gone is refused.
        let fourth = checked_write(temp.path(), &deletion("keep.txt", &third), b"").unwrap();
        assert!(entries(&fourth).is_empty());
        let missing = checked_write(temp.path(), &deletion("keep.txt", &fourth), b"").unwrap_err();
        assert_eq!(missing.kind, ErrorKind::Invalid);
    }

    #[test]
    fn symlink_blobs_can_be_deleted_but_not_edited() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(root.path()).unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("link", repo.blob(b"/outside/target").unwrap(), 0o120000)
            .unwrap();
        let tree = repo.find_tree(tree.write().unwrap()).unwrap();
        let author = git2::Signature::now("Tester", "test@example.invalid").unwrap();
        let sha = repo
            .commit(
                Some("refs/heads/main"),
                &author,
                &author,
                "symlink",
                &tree,
                &[],
            )
            .unwrap()
            .to_string();
        let mut write = deletion("link", &sha);
        write.deleting = false;
        assert_eq!(
            checked_write(root.path(), &write, b"replacement")
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
        let deleted = checked_write(root.path(), &deletion("link", &sha), b"").unwrap();
        assert!(repo
            .find_commit(git2::Oid::from_str(&deleted).unwrap())
            .unwrap()
            .tree()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn upload_indexes_a_large_directory_once_and_preserves_untouched_files() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(root.path()).unwrap();
        let blob = repo.blob(b"keep").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        for index in 0..100_000 {
            builder
                .insert(format!("file-{index:06}"), blob, 0o100644)
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let author = git2::Signature::now("Tester", "test@example.invalid").unwrap();
        let sha = repo
            .commit(
                Some("refs/heads/main"),
                &author,
                &author,
                "large directory",
                &tree,
                &[],
            )
            .unwrap();
        let write = UploadWrite {
            branch: "main".into(),
            message: "upload".into(),
            expected_revision: sha.to_string(),
            files: (0..100)
                .map(|i| UploadFile {
                    path: format!("file-{i:06}"),
                    size: 1,
                })
                .collect(),
        };
        let bytes = vec![b'x'; 100];
        let prepared = tokio::time::timeout(
            Duration::from_secs(30),
            prepare_upload(
                root.path(),
                &write,
                &bytes,
                "Tester",
                "test@example.invalid",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let tree = repo
            .find_commit(git2::Oid::from_str(&prepared.commit.sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        assert_eq!(tree.len(), 100_000);
        for file in &write.files {
            assert_eq!(
                repo.find_blob(tree.get_name(&file.path).unwrap().id())
                    .unwrap()
                    .content(),
                b"x"
            );
        }
        assert_eq!(tree.get_name("file-099999").unwrap().id(), blob);
    }

    #[test]
    fn a_delete_cannot_also_create() {
        let mut both = input("a.txt");
        both.deleting = true;
        assert!(both.validate().is_err());
    }

    #[test]
    fn stale_save_does_not_publish_or_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(temp.path()).unwrap();
        let mut value = input("docs/file.md");
        let first = checked_write(temp.path(), &value, b"one").unwrap();
        value.creating = false;
        value.expected_revision = first.clone();
        let second = checked_write(temp.path(), &value, b"two").unwrap();
        assert_eq!(
            checked_write(temp.path(), &value, b"stale")
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        let repo = git2::Repository::open_bare(temp.path()).unwrap();
        assert_eq!(
            repo.find_reference("refs/heads/main")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            second
        );
        let tree = repo
            .find_commit(git2::Oid::from_str(&second).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        let entry = tree.get_path(Path::new("docs/file.md")).unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), b"two");
    }

    #[test]
    fn competing_saves_have_one_cas_winner_and_preserve_binary_bytes() {
        let temp = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(temp.path()).unwrap();
        let mut value = input("docs/file.md");
        let first = checked_write(temp.path(), &value, b"one").unwrap();
        value.creating = false;
        value.expected_revision = first;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = [vec![0, 255, 1], vec![0, 255, 2]]
                .into_iter()
                .map(|bytes| {
                    let path = temp.path();
                    let value = value.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        (
                            checked_write_before_publish(path, &value, &bytes, || {
                                barrier.wait();
                            }),
                            bytes,
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            results.iter().filter(|(result, _)| result.is_ok()).count(),
            1
        );
        let (winner, bytes) = results.iter().find(|(result, _)| result.is_ok()).unwrap();
        let repo = git2::Repository::open_bare(temp.path()).unwrap();
        let commit = repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        assert_eq!(commit.id().to_string(), *winner.as_ref().unwrap());
        let entry = commit
            .tree()
            .unwrap()
            .get_path(Path::new("docs/file.md"))
            .unwrap();
        assert_eq!(
            repo.find_blob(entry.id()).unwrap().content(),
            bytes.as_slice()
        );
        assert!(results
            .iter()
            .filter_map(|(result, _)| result.as_ref().err())
            .all(|error| matches!(error.kind, ErrorKind::Conflict | ErrorKind::Unavailable)));
    }

    #[test]
    fn creation_refuses_existing_file_and_parent_file_collision() {
        let temp = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(temp.path()).unwrap();
        let mut value = input("docs/file.md");
        let first = checked_write(temp.path(), &value, b"one").unwrap();
        value.expected_revision = first;
        assert_eq!(
            checked_write(temp.path(), &value, b"one").unwrap_err().kind,
            ErrorKind::Invalid
        );
        value.path = "docs/file.md/child".into();
        assert_eq!(
            checked_write(temp.path(), &value, b"one").unwrap_err().kind,
            ErrorKind::Invalid
        );
    }

    #[tokio::test]
    async fn editor_preserves_executable_mode_and_literal_tab_newline_paths_and_neighbors() {
        let temp = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init_bare(temp.path()).unwrap();
        let mut request = input("bin/run");
        request.message = "descriptor save".into();
        let prepared = prepare(
            temp.path(),
            &request,
            b"script",
            "owner",
            "owner@example.test",
        )
        .await
        .unwrap();
        let first = publish(temp.path(), &prepared).await.unwrap();
        request.expected_revision = first.sha;
        request.path = "--literal\tline\nname".into();
        let prepared = prepare(
            temp.path(),
            &request,
            b"untouched",
            "owner",
            "owner@example.test",
        )
        .await
        .unwrap();
        let neighbor = publish(temp.path(), &prepared).await.unwrap();
        // Disposable fixture seeds a genuinely executable existing tree entry.
        let parent = repository
            .find_commit(git2::Oid::from_str(&neighbor.sha).unwrap())
            .unwrap();
        let root = parent.tree().unwrap();
        let bin = repository
            .find_tree(root.get_name("bin").unwrap().id())
            .unwrap();
        let script = bin.get_name("run").unwrap();
        let mut executable = repository.treebuilder(Some(&bin)).unwrap();
        executable.insert("run", script.id(), 0o100755).unwrap();
        let mut root_builder = repository.treebuilder(Some(&root)).unwrap();
        root_builder
            .insert("bin", executable.write().unwrap(), 0o040000)
            .unwrap();
        let tree = repository.find_tree(root_builder.write().unwrap()).unwrap();
        let author = git2::Signature::now("fixture", "fixture@example.test").unwrap();
        let revision = repository
            .commit(
                Some("refs/heads/main"),
                &author,
                &author,
                "executable fixture",
                &tree,
                &[&parent],
            )
            .unwrap();
        request.path = "bin/run".into();
        request.creating = false;
        request.expected_revision = revision.to_string();
        let prepared = prepare(
            temp.path(),
            &request,
            b"new\0\xff",
            "owner",
            "owner@example.test",
        )
        .await
        .unwrap();
        let saved = publish(temp.path(), &prepared).await.unwrap();
        let tree = repository
            .find_commit(git2::Oid::from_str(&saved.sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        assert_eq!(
            tree.get_path(Path::new("bin/run")).unwrap().filemode(),
            0o100755
        );
        assert_eq!(blob_at(temp.path(), "main", "bin/run"), b"new\0\xff");
        assert_eq!(
            blob_at(temp.path(), "main", "--literal\tline\nname"),
            b"untouched"
        );
        assert_eq!(saved.parents, vec![revision.to_string()]);
    }

    #[tokio::test]
    async fn editor_refuses_symbolic_branch_and_invalid_identity_without_ref_effects() {
        let temp = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init_bare(temp.path()).unwrap();
        let request = input("bin/run");
        assert_eq!(
            prepare(
                temp.path(),
                &request,
                b"bad",
                "bad\nidentity",
                "owner@example.test"
            )
            .await
            .err()
            .unwrap()
            .kind,
            ErrorKind::Invalid
        );
        repository
            .reference_symbolic(
                "refs/heads/main",
                "refs/heads/other",
                false,
                "fixture alias",
            )
            .unwrap();
        assert!(
            prepare(temp.path(), &request, b"bad", "owner", "owner@example.test")
                .await
                .is_err()
        );
        assert_eq!(
            repository
                .find_reference("refs/heads/main")
                .unwrap()
                .symbolic_target(),
            Some("refs/heads/other")
        );
        assert!(repository.find_reference("refs/heads/other").is_err());
    }

    fn upload(expected_revision: &str, files: &[(&str, usize)]) -> UploadWrite {
        UploadWrite {
            branch: "main".into(),
            message: "upload".into(),
            expected_revision: expected_revision.into(),
            files: files
                .iter()
                .map(|(path, size)| UploadFile {
                    path: (*path).into(),
                    size: *size,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn upload_lands_every_file_in_one_commit_and_replaces_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init_bare(temp.path()).unwrap();
        let first = upload("unborn", &[("one.txt", 3), ("docs/two.txt", 2)]);
        let prepared = prepare_upload(temp.path(), &first, b"abcde", "owner", "owner@example.test")
            .await
            .unwrap();
        let created = publish(temp.path(), &prepared).await.unwrap();
        assert!(created.parents.is_empty());
        // Replacing one file and adding another is a single commit on top.
        let second = upload(&created.sha, &[("one.txt", 2), ("three.txt", 1)]);
        let prepared = prepare_upload(temp.path(), &second, b"xyz", "owner", "owner@example.test")
            .await
            .unwrap();
        let saved = publish(temp.path(), &prepared).await.unwrap();
        assert_eq!(saved.parents, vec![created.sha.clone()]);
        let tree = repository
            .find_commit(git2::Oid::from_str(&saved.sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        for (path, bytes) in [
            ("one.txt", b"xy".as_slice()),
            ("docs/two.txt", b"de".as_slice()),
            ("three.txt", b"z".as_slice()),
        ] {
            let entry = tree.get_path(Path::new(path)).unwrap();
            assert_eq!(
                repository.find_blob(entry.id()).unwrap().content(),
                bytes,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn upload_refuses_unsafe_paths_duplicates_and_a_stale_head() {
        let temp = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(temp.path()).unwrap();
        for path in ["../x", "docs/../x", ".git/config", "a\\b", ""] {
            assert!(upload("unborn", &[(path, 1)]).validate().is_err(), "{path}");
        }
        assert!(upload("unborn", &[("a", 1), ("a", 1)]).validate().is_err());
        assert!(upload("unborn", &[("a", MAX_UPLOAD_FILE_BYTES + 1)])
            .validate()
            .is_err());
        let first = upload("unborn", &[("a.txt", 1)]);
        let prepared = prepare_upload(temp.path(), &first, b"a", "owner", "owner@example.test")
            .await
            .unwrap();
        publish(temp.path(), &prepared).await.unwrap();
        let stale = upload("unborn", &[("b.txt", 1)]);
        let error = prepare_upload(temp.path(), &stale, b"b", "owner", "owner@example.test")
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::Conflict);
    }
}
