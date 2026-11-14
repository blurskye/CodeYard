//! Merge commits, repository creation and CI checkouts from exact revisions.
//! Ref updates go through `update-ref` transactions that check the old value.
use super::{
    durable, find_commit_checked, open_quarantined, process, COMMIT_WALK_TIMEOUT,
    MAX_COMMIT_WALK_BYTES,
};
use anyhow::{anyhow, bail, ensure, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(300);
pub use codeyard_types::git_limits::MAX_MERGE_MESSAGE_BYTES;
/// Conflicting paths that a refusal names. The rest are counted.
const LISTED_CONFLICTS: usize = 20;

/// Shown when a merge would add nothing to the base branch.
pub const NO_CHANGES: &str = "The pull request adds no changes to the base branch.";

/// Someone who writes or commits a commit. An author date keeps the date of the commit the
/// author came from, in Git's `<seconds> <offset>` form.
pub struct Person<'a> {
    pub name: &'a str,
    pub email: &'a str,
    pub date: Option<&'a str>,
}

/// The branches conflict. The message names the conflicting files.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct MergeConflict(pub(crate) String);

/// The commits cannot be merged this way, for example because they contain merge commits.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UnsupportedMerge(pub String);

pub fn validate_merge_revision(branch: &str, sha: &str) -> Result<()> {
    if crate::branch_ref(branch).is_none() || !crate::is_full_commit_id(sha) {
        bail!("A valid branch and complete expected revision are required");
    }
    Ok(())
}

/// Runs `git` on the repository at `path`. A command that writes a commit passes the author
/// and the committer, so Git never takes their names from the machine.
pub(super) async fn command(
    path: &Path,
    args: &[&str],
    input: &[u8],
    people: Option<(&Person<'_>, &Person<'_>)>,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut command = process::git_command()?;
    command.arg("--git-dir").arg(path).args(args);
    if let Some((author, committer)) = people {
        command
            .env("GIT_AUTHOR_NAME", author.name)
            .env("GIT_AUTHOR_EMAIL", author.email)
            .env("GIT_COMMITTER_NAME", committer.name)
            .env("GIT_COMMITTER_EMAIL", committer.email);
        if let Some(date) = author.date {
            command.env("GIT_AUTHOR_DATE", date);
        }
    }
    process::buffered(command, input, MAX_OUTPUT, COMMAND_TIMEOUT).await
}

fn oid_output(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes)?.trim();
    if !crate::is_full_commit_id(text) {
        bail!("Git returned an invalid object ID");
    }
    Ok(text.to_ascii_lowercase())
}

pub async fn commit_is_ancestor(path: &Path, base: &str, head: &str) -> Result<bool> {
    validate_merge_revision("main", base)?;
    validate_merge_revision("main", head)?;
    let (status, _) = command(
        path,
        &["merge-base", "--is-ancestor", base, head],
        b"",
        None,
    )
    .await?;
    ancestry_answer(status)
}

/// `git merge-base --is-ancestor` exits 0 for yes and 1 for no. Anything else is a failure.
fn ancestry_answer(status: std::process::ExitStatus) -> Result<bool> {
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("Git ancestry check failed"),
    }
}

/// Quotes a path as one C-style string, so Git reads it as a single entry both in
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES` (split on `:`) and in an `alternates` file (one entry
/// per line).
pub(super) fn quote_alternate(objects: &Path) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut quoted = vec![b'"'];
    for &byte in objects.as_os_str().as_bytes() {
        match byte {
            b'\\' | b'"' => quoted.extend_from_slice(&[b'\\', byte]),
            0..=0x1f | 0x7f => quoted.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
            _ => quoted.push(byte),
        }
    }
    quoted.push(b'"');
    std::ffi::OsString::from_vec(quoted)
}

/// A `git` command against `path` that also reads the objects of a push Git has not yet
/// accepted. Receive-pack keeps them in `quarantine`, which is not part of the repository's
/// own object directory until the push is accepted.
pub(crate) fn quarantined_command(
    path: &Path,
    quarantine: Option<&Path>,
) -> Result<tokio::process::Command> {
    let mut command = process::git_command()?;
    command.arg("--git-dir").arg(path);
    if let Some(quarantine) = quarantine {
        command.env("GIT_OBJECT_DIRECTORY", quarantine).env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            quote_alternate(&path.join("objects")),
        );
    }
    Ok(command)
}

async fn quarantined(
    path: &Path,
    quarantine: &Path,
    args: &[&str],
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut command = quarantined_command(path, Some(quarantine))?;
    command.args(args);
    process::buffered(command, b"", MAX_OUTPUT, COMMAND_TIMEOUT).await
}

/// Whether moving a branch from `old` to `new` only adds commits, for a push still in
/// quarantine.
pub async fn quarantined_is_fast_forward(
    path: &Path,
    quarantine: &Path,
    old: &str,
    new: &str,
) -> Result<bool> {
    validate_merge_revision("main", old)?;
    validate_merge_revision("main", new)?;
    let (status, _) =
        quarantined(path, quarantine, &["merge-base", "--is-ancestor", old, new]).await?;
    ancestry_answer(status)
}

/// Whether `new` brings a merge commit that `old` does not have. A new branch checks
/// its whole history, even when another ref already holds it. A push still in
/// `quarantine` is read through it; a prepared merge is already in the repository.
pub async fn adds_merge_commit(
    path: &Path,
    quarantine: Option<&Path>,
    old: Option<&str>,
    new: &str,
) -> Result<bool> {
    validate_merge_revision("main", new)?;
    let mut args = vec!["rev-list", "--merges", "-n1", new];
    if let Some(old) = old {
        validate_merge_revision("main", old)?;
        args.extend(["--not", old]);
    }
    let (status, output) = match quarantine {
        Some(quarantine) => quarantined(path, quarantine, &args).await?,
        None => command(path, &args, b"", None).await?,
    };
    ensure!(status.success(), "Git merge commit check failed");
    Ok(!output.is_empty())
}

/// One introduced commit's signature, the bytes it covers and the committer address. The
/// payload stays raw: a signature covers exact bytes, which lossy UTF-8 decoding can change.
pub struct CommitSignature {
    pub signature: Option<String>,
    pub payload: Option<Vec<u8>>,
    pub committer_email: String,
}

/// Commits a signed-commit check reads at most; a longer history is refused, not guessed.
const MAX_SIGNATURE_CHECKS: usize = 10_000;

/// The signatures of the commits `new` adds over `old`, for the signed-commit rule.
pub async fn introduced_signatures(
    path: &Path,
    quarantine: Option<&Path>,
    old: Option<&str>,
    new: &str,
) -> Result<Vec<CommitSignature>> {
    validate_merge_revision("main", new)?;
    if let Some(old) = old {
        validate_merge_revision("main", old)?;
    }
    let commits = walk_commits(
        path,
        quarantine,
        &[new.to_owned()],
        &old.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        false,
        MAX_SIGNATURE_CHECKS,
        COMMIT_WALK_TIMEOUT,
    )
    .await?;
    let path = path.to_owned();
    let quarantine = quarantine.map(Path::to_owned);
    let deadline = Instant::now() + COMMIT_WALK_TIMEOUT;
    process::blocking_until(deadline, move || -> Result<Vec<CommitSignature>> {
        let repo = open_quarantined(&path, quarantine.as_deref())?;
        let mut signatures = Vec::new();
        // The payloads are kept until every signature is checked, so their total is bounded.
        let mut bytes = 0usize;
        for oid in commits {
            ensure!(
                Instant::now() < deadline,
                "Checking the commit signatures took too long"
            );
            bytes = bytes.saturating_add(repo.odb()?.read_header(oid)?.0);
            ensure!(
                bytes <= MAX_COMMIT_WALK_BYTES,
                "Too many commit bytes to check their signatures"
            );
            // Refuses an oversized commit before its payload is copied.
            let commit = find_commit_checked(&repo, oid)?;
            let (signature, payload) = match super::extract_commit_signature(&repo, &commit) {
                Ok((signature, payload)) => (
                    Some(String::from_utf8_lossy(&signature).into_owned()),
                    Some(payload.to_vec()),
                ),
                Err(error) if error.code() == git2::ErrorCode::NotFound => (None, None),
                Err(error) => return Err(error.into()),
            };
            signatures.push(CommitSignature {
                signature,
                payload,
                committer_email: String::from_utf8_lossy(commit.committer().email_bytes())
                    .into_owned(),
            });
        }
        Ok(signatures)
    })
    .await
}

/// Enumerate IDs in a memory-limited child; libgit2 revwalk materializes unchecked ancestors.
pub(crate) async fn walk_commits(
    path: &Path,
    quarantine: Option<&Path>,
    tips: &[String],
    hidden: &[String],
    hide_published: bool,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<git2::Oid>> {
    for id in tips.iter().chain(hidden) {
        validate_merge_revision("main", id)?;
    }
    let mut command = quarantined_command(path, quarantine)?;
    command
        .args(["rev-list", &format!("--max-count={}", limit + 1)])
        .args(tips);
    if !hidden.is_empty() || hide_published {
        command.arg("--not").args(hidden);
        if hide_published {
            for namespace in crate::ref_names::PUBLIC_REF_NAMESPACES {
                command.arg(format!("--glob={namespace}*"));
            }
        }
    }
    command.arg("--");
    let (status, bytes) = process::buffered(command, b"", (limit + 1) * 41, timeout).await?;
    ensure!(status.success(), "Git could not enumerate commits");
    let ids = std::str::from_utf8(&bytes)?
        .lines()
        .map(git2::Oid::from_str)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(ids.len() <= limit, "Too many commits to scan");
    Ok(ids)
}

/// Builds a two-parent merge commit of `base` and `head` without touching refs, even when `head`
/// already contains `base`. Conflicts are a [`MergeConflict`].
pub async fn prepare_merge(
    path: &Path,
    head: &str,
    base: &str,
    message: &str,
    author: &Person<'_>,
    committer: &Person<'_>,
) -> Result<String> {
    validate_merge_revision("main", head)?;
    validate_merge_revision("main", base)?;
    check_commit_text(message, &[author, committer])?;
    if commit_is_ancestor(path, head, base).await? {
        return Err(UnsupportedMerge(NO_CHANGES.to_owned()).into());
    }
    let tree = merge_tree(path, base, head).await?;
    commit_tree(
        path,
        &tree,
        &[base, head],
        message.as_bytes(),
        author,
        committer,
    )
    .await
}

/// Builds one commit on top of `base` that holds the whole change of `head`. `author` wrote it
/// and `committer` commits it. Without a supplied body, the title is followed by the subjects of
/// the squashed commits, cut to the size limit. An unchanged base tree is refused.
pub async fn prepare_squash(
    path: &Path,
    head: &str,
    base: &str,
    title: &str,
    body: Option<&str>,
    author: &Person<'_>,
    committer: &Person<'_>,
) -> Result<String> {
    validate_merge_revision("main", head)?;
    validate_merge_revision("main", base)?;
    check_commit_text(title, &[author, committer])?;
    let tree = merge_tree(path, base, head).await?;
    if tree == tree_of(path, base).await? {
        return Err(UnsupportedMerge(NO_CHANGES.to_owned()).into());
    }
    let message = match body {
        Some("") => title.to_owned(),
        Some(body) => format!("{title}\n\n{body}"),
        None => squash_message(path, base, head, title).await?,
    };
    check_commit_text(&message, &[author, committer])?;
    commit_tree(path, &tree, &[base], message.as_bytes(), author, committer).await
}

/// `title`, then a `* subject` line for each commit in `base..head`, oldest first, for as many
/// as fit in the message limit.
async fn squash_message(path: &Path, base: &str, head: &str, title: &str) -> Result<String> {
    let range = format!("{base}..{head}");
    let mut command = process::git_command()?;
    command
        .arg("--git-dir")
        .arg(path)
        .args([
            "log",
            "--reverse",
            "--max-count=100",
            "-z",
            "--format=%s",
            &range,
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (_, listed) = process::read_stream(command, b"", COMMAND_TIMEOUT, |stdout, _| async move {
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut listed = String::new();
        while let Some((subject, _)) = super::read_delimited(&mut reader, 0, 100).await? {
            let subject = String::from_utf8_lossy(&subject);
            let mut end = subject.len().min(100);
            while !subject.is_char_boundary(end) {
                end -= 1;
            }
            let line = format!("* {}\n", subject[..end].trim_end_matches(['\0', '\n', ' ']));
            if title.len() + 2 + listed.len() + line.len() > MAX_MERGE_MESSAGE_BYTES {
                return Ok(process::ReadEnd::Prefix(listed));
            }
            listed.push_str(&line);
        }
        Ok(process::ReadEnd::Complete(listed))
    })
    .await?;
    if listed.is_empty() {
        return Ok(title.to_owned());
    }
    Ok(format!("{title}\n\n{listed}"))
}

/// Refuses a message that a commit cannot carry, or a name or email that Git would reject. A merge
/// or squash message is at most `MAX_MERGE_MESSAGE_BYTES`, and an identity is at most
/// `MAX_COMMIT_NAME_BYTES` or `MAX_COMMIT_EMAIL_BYTES`, so those commits stay far below
/// `MAX_COMMIT_OBJECT_BYTES`. Only a replayed commit, which keeps its own message, can outgrow it,
/// and the replay checks that.
fn check_commit_text(message: &str, people: &[&Person<'_>]) -> Result<()> {
    for person in people {
        crate::objects::validate_identity(person.name, person.email)?;
    }
    if message.is_empty() || message.contains('\0') || message.len() > MAX_MERGE_MESSAGE_BYTES {
        return Err(crate::error::RepoError::new(
            crate::error::ErrorKind::Invalid,
            "Invalid merge commit identity or message",
        )
        .into());
    }
    Ok(())
}

/// The tree that merging `head` into `base` gives. A conflict is a [`MergeConflict`].
async fn merge_tree(path: &Path, base: &str, head: &str) -> Result<String> {
    let (status, output) = command(
        path,
        &["merge-tree", "--write-tree", "--name-only", base, head],
        b"",
        None,
    )
    .await?;
    merge_tree_result(status, &output)
}

/// The tree that merging `theirs` into `ours` gives, with `base` as the merge base. A conflict is
/// a [`MergeConflict`].
pub(crate) async fn merge_tree_with_base(
    path: &Path,
    base: &str,
    ours: &str,
    theirs: &str,
) -> Result<String> {
    let merge_base = format!("--merge-base={base}");
    let (status, output) = command(
        path,
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            &merge_base,
            ours,
            theirs,
        ],
        b"",
        None,
    )
    .await?;
    merge_tree_result(status, &output)
}

/// Reads the output of `git merge-tree --write-tree --name-only`. Exit status 0 prints the
/// result tree. Exit status 1 is a conflict: the conflicting paths follow the tree line, up to a
/// blank line.
pub(crate) fn merge_tree_result(status: std::process::ExitStatus, output: &[u8]) -> Result<String> {
    match status.code() {
        Some(0) => oid_output(output),
        Some(1) => {
            let text = std::str::from_utf8(output)?;
            let files: Vec<&str> = text
                .lines()
                .skip(1)
                .take_while(|line| !line.is_empty())
                .collect();
            Err(MergeConflict(conflict_message(&files)).into())
        }
        _ => bail!("Git could not merge the trees"),
    }
}

fn conflict_message(files: &[&str]) -> String {
    let mut message = "The branches conflict and can't be merged automatically.".to_owned();
    if !files.is_empty() {
        let listed: Vec<&str> = files.iter().take(LISTED_CONFLICTS).copied().collect();
        message.push_str(" Conflicting files: ");
        message.push_str(&listed.join(", "));
        if files.len() > LISTED_CONFLICTS {
            message.push_str(&format!(" and {} more", files.len() - LISTED_CONFLICTS));
        }
        message.push('.');
    }
    message
}

/// The tree of `commit`.
pub(crate) async fn tree_of(path: &Path, commit: &str) -> Result<String> {
    validate_merge_revision("main", commit)?;
    let spec = format!("{commit}^{{tree}}");
    let (status, output) = command(path, &["rev-parse", "--verify", &spec], b"", None).await?;
    ensure!(status.success(), "Git could not read a commit's tree");
    oid_output(&output)
}

/// Writes a commit of `tree` on `parents` and returns its ID. The author and the committer are
/// named from `author` and `committer`, and the author date is kept when `author` has one.
pub(crate) async fn commit_tree(
    path: &Path,
    tree: &str,
    parents: &[&str],
    message: &[u8],
    author: &Person<'_>,
    committer: &Person<'_>,
) -> Result<String> {
    commit_tree_bytes(
        path,
        tree,
        parents,
        message,
        std::ffi::OsStr::new(author.name),
        std::ffi::OsStr::new(author.email),
        author.date,
        committer,
    )
    .await
}

/// Legacy authors need not be UTF-8; the environment carries their original bytes.
#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one input of git commit-tree."
)]
pub(super) async fn commit_tree_bytes(
    path: &Path,
    tree: &str,
    parents: &[&str],
    message: &[u8],
    author_name: &std::ffi::OsStr,
    author_email: &std::ffi::OsStr,
    author_date: Option<&str>,
    committer: &Person<'_>,
) -> Result<String> {
    let mut command = process::git_command()?;
    command
        .arg("--git-dir")
        .arg(path)
        .args(["commit-tree", tree]);
    for parent in parents {
        command.args(["-p", *parent]);
    }
    command
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_COMMITTER_NAME", committer.name)
        .env("GIT_COMMITTER_EMAIL", committer.email);
    if let Some(date) = author_date {
        command.env("GIT_AUTHOR_DATE", date);
    }
    let (status, output) = process::buffered(command, message, MAX_OUTPUT, COMMAND_TIMEOUT).await?;
    ensure!(status.success(), "Git failed to construct the commit");
    oid_output(&output)
}

/// Creates a durable bare repository. Forks fetch refs into a fresh repository instead
/// of copying the source's config, hooks or alternates.
pub async fn initialize_repository(
    path: &Path,
    source: Option<&Path>,
    default_branch: &str,
) -> Result<()> {
    initialize_repository_copy(path, source, default_branch, false, false, None).await
}

/// Forks of one public repository on one disk share its objects through a pool repository at
/// `<root>/pools/<name>.git`, next to the forks in `<root>`. The pool holds only what the
/// source's branches and tags reach, keeps those refs as fetch bases and never prunes, so a
/// member never loses a borrowed object and stores only the objects it adds itself.
async fn join_pool(path: &Path, pool: &Path, source: &str) -> Result<()> {
    let name = pool
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.ends_with(".git") && !name.starts_with('.'))
        .ok_or_else(|| anyhow!("Invalid object pool path"))?;
    let pools = pool
        .parent()
        .ok_or_else(|| anyhow!("Invalid object pool path"))?;
    ensure!(
        pools.file_name() == Some(std::ffi::OsStr::new("pools")) && pools.parent() == path.parent(),
        "The object pool is not beside the repository"
    );
    let _lock = super::locks::lock(pool).await;
    if !tokio::fs::try_exists(pool.join("objects")).await? {
        let directory = pools.to_owned();
        process::blocking_until(Instant::now() + COMMAND_TIMEOUT, move || {
            codeyard_security::fs::durable_dir(&directory)?;
            Ok(())
        })
        .await?;
        // A pool appears only complete, with its no-prune settings, so no Git command ever
        // collects a member's objects from it.
        let staging = tempfile::Builder::new()
            .prefix(".pool-")
            .tempdir_in(pools)?;
        let (status, _) =
            command(staging.path(), &["init", "--bare", "--quiet"], b"", None).await?;
        ensure!(status.success(), "Git object pool initialization failed");
        for (key, value) in [
            ("gc.auto", "0"),
            ("gc.pruneExpire", "never"),
            ("gc.reflogExpireUnreachable", "never"),
            ("maintenance.auto", "false"),
            ("transfer.unpackLimit", "1"),
            ("core.logAllRefUpdates", "false"),
        ] {
            let (status, _) = command(staging.path(), &["config", key, value], b"", None).await?;
            ensure!(status.success(), "Git object pool configuration failed");
        }
        sync_initialized_repository(staging.path()).await?;
        tokio::fs::rename(staging.path(), pool).await?;
        durable::sync_ancestors(pool, pools, Instant::now() + COMMAND_TIMEOUT).await?;
    }
    let (status, _) = command(
        pool,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            source,
            "+refs/heads/*:refs/heads/*",
            "+refs/tags/*:refs/tags/*",
        ],
        b"",
        None,
    )
    .await?;
    ensure!(status.success(), "Git object pool update failed");
    // A member borrows whatever the pool holds, so the pool's refs and object entries must
    // be durable before a member ref is.
    sync_fetched_refs(pool, Instant::now() + COMMAND_TIMEOUT).await?;
    // Relative, so a backup or a restored copy of the whole storage root keeps working.
    tokio::fs::write(
        path.join("objects/info/alternates"),
        format!("../../pools/{name}/objects\n"),
    )
    .await?;
    Ok(())
}

pub async fn initialize_repository_copy(
    path: &Path,
    source: Option<&Path>,
    default_branch: &str,
    default_branch_only: bool,
    template: bool,
    pool: Option<&Path>,
) -> Result<()> {
    validate_merge_revision(default_branch, "0000000000000000000000000000000000000000")?;
    let branch = format!("--initial-branch={default_branch}");
    let (status, _) = command(path, &["init", "--bare", &branch], b"", None).await?;
    if !status.success() {
        bail!("Git repository initialization failed");
    }
    let fetched = source.is_some();
    if let Some(source) = source {
        ensure!(
            tokio::fs::try_exists(source).await?,
            "Source repository is unavailable"
        );
        let source = source
            .to_str()
            .ok_or_else(|| anyhow!("Invalid source storage path"))?;
        if let Some(pool) = pool.filter(|_| !template) {
            join_pool(path, pool, source).await?;
        }
        let default_ref = format!("+refs/heads/{default_branch}:refs/heads/{default_branch}");
        let mut arguments = vec!["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"];
        if template {
            arguments.push("--depth=1");
        }
        arguments.extend(["--", source]);
        if default_branch_only {
            arguments.push(&default_ref);
        } else {
            arguments.extend(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]);
        }
        let (status, _) = command(path, &arguments, b"", None).await?;
        if !status.success() {
            bail!("Git fork snapshot failed");
        }
        let (status, heads) = command(
            path,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
            b"",
            None,
        )
        .await?;
        ensure!(status.success(), "Git fork branch listing failed");
        let expected = format!("refs/heads/{default_branch}");
        ensure!(
            heads.is_empty()
                || heads
                    .split(|byte| *byte == b'\n')
                    .any(|name| name == expected.as_bytes()),
            "The source default branch changed while copying"
        );
    }
    sync_initialized_repository(path).await?;
    if fetched {
        sync_fetched_refs(path, Instant::now() + COMMAND_TIMEOUT).await?;
    }
    Ok(())
}

// core.fsync=all covers Git objects and refs, but not the HEAD/config created by init
// or an alternates file we write. Sync that bounded metadata set, and the directory that
// names the new repository, before publishing it. The fixed ref directories are required:
// a rename replaces their entries, so a missing one is a failure, not a skip.
async fn sync_initialized_repository(path: &Path) -> Result<()> {
    durable::RepositorySync::new()
        .required_file("HEAD")
        .required_file("config")
        .optional_file("shallow")
        .optional_file("objects/info/alternates")
        .required_dir("refs")
        .required_dir("refs/heads")
        .required_dir("refs/tags")
        .objects()
        .parent()
        .sync(path, Instant::now() + COMMAND_TIMEOUT)
        .await
}

/// Makes the refs a fetch just wrote durable, including nested names and the object entries
/// the transfer created. `sync_initialized_repository` covers only the fixed init set, and a
/// fetch can write `refs/heads/topic/nested` and a new pack.
async fn sync_fetched_refs(path: &Path, deadline: Instant) -> Result<()> {
    let owned = path.to_owned();
    let refs = process::blocking_until(deadline, move || super::list_mirror_refs(&owned)).await?;
    durable::sync_refs(path, refs.into_keys().collect(), deadline).await
}

/// The refs of a repository with their objects, and its HEAD, as Git prints them.
async fn ref_listing(path: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let (status, refs) = command(
        path,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
        b"",
        None,
    )
    .await?;
    ensure!(status.success(), "Git ref listing failed");
    let (status, head) = command(path, &["symbolic-ref", "HEAD"], b"", None).await?;
    ensure!(status.success(), "Git HEAD is not a branch");
    Ok((refs, head))
}

/// Copies every ref of `source`, with the objects they reach, into a new bare repository at
/// `path`, sets the same HEAD, and checks that both list the same refs at the same objects.
/// Git checks each object it receives. An account move copies repositories this way.
pub async fn copy_repository(path: &Path, source: &Path, timeout: Duration) -> Result<()> {
    let source_text = source
        .to_str()
        .ok_or_else(|| anyhow!("Invalid source storage path"))?;
    let (_, head) = ref_listing(source).await?;
    let head = std::str::from_utf8(&head)?.trim().to_owned();
    let branch = head
        .strip_prefix("refs/heads/")
        .ok_or_else(|| anyhow!("Git HEAD is not a branch"))?;
    initialize_repository(path, None, branch).await?;
    let mut fetch = process::git_command()?;
    fetch.arg("--git-dir").arg(path).args([
        "-c",
        "fetch.fsckObjects=true",
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-write-fetch-head",
        "--",
        source_text,
        "+refs/*:refs/*",
    ]);
    let (status, _) = process::buffered(fetch, b"", MAX_OUTPUT, timeout).await?;
    ensure!(status.success(), "Git repository copy failed");
    same_refs(path, source).await?;
    sync_fetched_refs(path, Instant::now() + timeout).await
}

/// Fails unless both repositories list the same refs at the same objects and the same HEAD.
pub async fn same_refs(path: &Path, source: &Path) -> Result<()> {
    ensure!(
        ref_listing(path).await? == ref_listing(source).await?,
        "The repository copy does not match its source"
    );
    Ok(())
}

/// A template copies trees, not history. Prune before publication so old private commits cannot
/// be recovered through object-ID reads on the generated repository.
pub async fn rewrite_template_history(
    path: &Path,
    default_branch: &str,
    author: (&str, &str),
) -> Result<()> {
    let (path_owned, branch, name, email) = (
        path.to_owned(),
        default_branch.to_owned(),
        author.0.to_owned(),
        author.1.to_owned(),
    );
    process::blocking_until(
        Instant::now() + process::RPC_TIMEOUT,
        move || -> Result<()> {
            let repo = git2::Repository::open_bare(&path_owned)?;
            let signature = git2::Signature::now(&name, &email)?;
            super::peel_to_commit_checked(&repo, &format!("refs/heads/{branch}"))?;
            let mut names = Vec::new();
            let mut bytes = 0;
            for reference in repo.references()? {
                let reference = reference?;
                let name = reference
                    .name()
                    .map_err(|_| anyhow!("Invalid reference name"))?
                    .to_owned();
                bytes += name.len();
                if names.len() >= super::MAX_REPOSITORY_REFS
                    || bytes > super::MAX_REPOSITORY_REF_BYTES
                {
                    return Err(super::ObjectTooLarge.into());
                }
                names.push(name);
            }
            for name in names {
                if !name.starts_with("refs/heads/") {
                    repo.find_reference(&name)?.delete()?;
                    continue;
                }
                let commit = super::peel_to_commit_checked(&repo, &name)?;
                let tree = super::directory_tree(&repo, &commit, "")?;
                let message = format!(
                    "Initial commit from {}",
                    name.trim_start_matches("refs/heads/")
                );
                let id = repo.commit(None, &signature, &signature, &message, &tree, &[])?;
                repo.reference(&name, id, true, "Create from template")?;
            }
            repo.set_head(&format!("refs/heads/{branch}"))?;
            Ok(())
        },
    )
    .await?;
    match tokio::fs::remove_file(path.join("shallow")).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for arguments in [
        &["reflog", "expire", "--expire=now", "--all"][..],
        &[
            "-c",
            "gc.autoDetach=false",
            "-c",
            "gc.bigPackThreshold=0",
            "-c",
            "gc.cruftPacks=false",
            "gc",
            "--prune=now",
            "--quiet",
        ][..],
    ] {
        let (status, _) = command(path, arguments, b"", None).await?;
        ensure!(status.success(), "Git template history cleanup failed");
    }
    let (status, output) = command(
        path,
        &["fsck", "--no-reflogs", "--unreachable", "--no-progress"],
        b"",
        None,
    )
    .await?;
    ensure!(
        status.success() && output.is_empty(),
        "Template contains unreachable history"
    );
    // The ref rewrites and `gc` changed ref files, the directories that hold them and the
    // object entries of the repacked store. None of that is synced by Git, so make the
    // finished template durable before it is copied or published.
    sync_fetched_refs(path, Instant::now() + COMMAND_TIMEOUT).await
}

#[cfg(test)]
mod pull_head_tests {
    use super::*;

    #[tokio::test]
    async fn changed_pull_heads_borrow_existing_base_objects_for_fetch_negotiation() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("base.git");
        let source = root.path().join("source.git");
        initialize_repository(&base, None, "main").await.unwrap();
        let repo = git2::Repository::open_bare(&base).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let large = repo.blob(&vec![b'x'; 1024 * 1024]).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("large.txt", large, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let first = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "base",
                &tree,
                &[],
            )
            .unwrap();
        initialize_repository(&source, Some(&base), "main")
            .await
            .unwrap();
        let fork = git2::Repository::open_bare(&source).unwrap();
        let parent = fork.find_commit(first).unwrap();
        let mut builder = fork.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        builder
            .insert("small.txt", fork.blob(b"new").unwrap(), 0o100644)
            .unwrap();
        let tree = fork.find_tree(builder.write().unwrap()).unwrap();
        let head = fork
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "new head",
                &tree,
                &[&parent],
            )
            .unwrap();
        let prepared = prepare_pull_head(&base, &source, &head.to_string())
            .await
            .unwrap();
        std::fs::remove_file(prepared.path().join("objects/info/alternates")).unwrap();
        let temporary = git2::Repository::open_bare(prepared.path()).unwrap();
        assert!(temporary.odb().unwrap().exists(head));
        assert!(
            !temporary.odb().unwrap().exists(large),
            "the existing base blob must not be fetched again"
        );
        assert!(
            !repo.odb().unwrap().exists(head),
            "preparation must not publish fork objects"
        );
    }
}

const TEMPORARY_PULL_HEAD: &str = "refs/heads/pull-head";

/// A fork head fetched into an isolated repository. Dropping it removes every
/// fetched object without exposing it through the base repository.
pub struct PreparedPullHead {
    temporary: tempfile::TempDir,
    head: String,
}

impl PreparedPullHead {
    pub fn path(&self) -> &Path {
        self.temporary.path()
    }
    pub fn head(&self) -> &str {
        &self.head
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Git pull request ref changed")]
pub struct PullHeadConflict;

/// Fetches a local fork commit into temporary storage. The base repository is
/// not touched until [`publish_pull_head`] runs after the live authority check.
pub async fn prepare_pull_head(base: &Path, source: &Path, head: &str) -> Result<PreparedPullHead> {
    validate_merge_revision("main", head)?;
    let (base, source) = (base.to_owned(), source.to_owned());
    let (source, temporary, mut alternates, refs) = process::blocking_until(
        std::time::Instant::now() + process::RPC_TIMEOUT,
        move || {
            use std::os::unix::ffi::OsStringExt;
            let source = std::path::absolute(source)?;
            ensure!(source.is_dir(), "Repository storage is unavailable");
            let source = source
                .to_str()
                .ok_or_else(|| anyhow!("Invalid source storage path"))?
                .to_owned();
            let temporary = tempfile::tempdir()?;
            let base = std::fs::canonicalize(base)?;
            let alternates = quote_alternate(&base.join("objects")).into_vec();
            let refs = super::list_mirror_refs(&base)?;
            Ok((source, temporary, alternates, refs))
        },
    )
    .await?;
    initialize_repository(temporary.path(), None, "main").await?;
    alternates.push(b'\n');
    tokio::fs::write(temporary.path().join("objects/info/alternates"), alternates).await?;
    let mut script = String::from("start\n");
    for (index, target) in refs.values().enumerate() {
        script.push_str(&format!("update refs/known/{index} {target}\n"));
    }
    script.push_str("prepare\ncommit\n");
    let (status, _) = command(
        temporary.path(),
        &["update-ref", "--stdin"],
        script.as_bytes(),
        None,
    )
    .await?;
    ensure!(
        status.success(),
        "Git pull request negotiation setup failed"
    );
    let refspec = format!("+{head}:{TEMPORARY_PULL_HEAD}");
    let (status, _) = command(
        temporary.path(),
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            &source,
            &refspec,
        ],
        b"",
        None,
    )
    .await?;
    ensure!(status.success(), "Git pull request fetch failed");
    let (status, fetched) = command(
        temporary.path(),
        &["rev-parse", "--verify", TEMPORARY_PULL_HEAD],
        b"",
        None,
    )
    .await?;
    ensure!(
        status.success() && oid_output(&fetched)? == head,
        "Git pull request fetch returned the wrong commit"
    );
    Ok(PreparedPullHead {
        temporary,
        head: head.to_owned(),
    })
}

pub async fn prepare_pull_branch_update(
    path: &Path,
    source: &Path,
    head: &str,
    base: &str,
    rebase: bool,
    committer: &Person<'_>,
) -> Result<PreparedPullHead> {
    let mut prepared = prepare_pull_head(path, source, base).await?;
    let result = if commit_is_ancestor(prepared.path(), base, head).await? {
        head.to_owned()
    } else if rebase {
        super::replay_commits(prepared.path(), base, head, base, committer)
            .await?
            .tip
    } else {
        prepare_merge(
            prepared.path(),
            base,
            head,
            "Merge base branch into pull request head",
            committer,
            committer,
        )
        .await?
    };
    publish_ref(
        prepared.path(),
        "update",
        TEMPORARY_PULL_HEAD,
        Some(&result),
        base,
    )
    .await?;
    prepared.head = result;
    Ok(prepared)
}

/// Imports the upstream tip without publishing it, preserving a fork's own commits.
pub async fn prepare_upstream_merge(
    path: &Path,
    source: &Path,
    head: &str,
    upstream: &str,
    committer: &Person<'_>,
) -> Result<PreparedPullHead> {
    let mut prepared = prepare_pull_head(path, source, upstream).await?;
    let result = if commit_is_ancestor(prepared.path(), upstream, head).await? {
        head.to_owned()
    } else if commit_is_ancestor(prepared.path(), head, upstream).await? {
        upstream.to_owned()
    } else {
        prepare_merge(
            prepared.path(),
            upstream,
            head,
            "Merge upstream branch",
            committer,
            committer,
        )
        .await?
    };
    if result != upstream {
        publish_ref(
            prepared.path(),
            "sync fork",
            TEMPORARY_PULL_HEAD,
            Some(&result),
            upstream,
        )
        .await?;
    }
    prepared.head = result;
    Ok(prepared)
}

fn is_pull_head_ref(reference: &str) -> bool {
    reference
        .strip_prefix("refs/pull/")
        .and_then(|tail| tail.strip_suffix("/head"))
        .and_then(|number| number.parse::<i32>().ok())
        .is_some_and(|number| number > 0)
        && git2::Reference::is_valid_name(reference)
}

/// The hidden ref that keeps a Git-data object no branch or tag names: `refs/codeyard/objects/`
/// followed by the object's full ID.
fn is_retained_object_ref(reference: &str) -> bool {
    reference
        .strip_prefix("refs/codeyard/objects/")
        .is_some_and(|id| {
            codeyard_types::git_ids::is_full_commit_id(id)
                && !id.bytes().any(|byte| byte.is_ascii_uppercase())
        })
}

pub async fn current_pull_head(path: &Path, number: i32) -> Result<Option<String>> {
    ensure!(number > 0, "Invalid pull request number");
    let reference = format!("refs/pull/{number}/head");
    ensure!(
        is_pull_head_ref(&reference),
        "Invalid pull request reference"
    );
    current_reference(path, &reference).await
}

pub async fn current_branch_head(path: &Path, branch: &str) -> Result<Option<String>> {
    let reference = crate::branch_ref(branch).ok_or_else(|| anyhow!("Invalid branch"))?;
    current_reference(path, &reference).await
}

async fn current_reference(path: &Path, reference: &str) -> Result<Option<String>> {
    ensure!(
        crate::is_canonical_ref(reference)
            || is_pull_head_ref(reference)
            || is_retained_object_ref(reference),
        "Invalid reference"
    );
    let (status, current) = command(
        path,
        &["rev-parse", "--verify", "--quiet", reference],
        b"",
        None,
    )
    .await?;
    match status.code() {
        Some(0) => Ok(Some(oid_output(&current)?)),
        Some(1) => Ok(None),
        _ => bail!("Git pull request ref lookup failed"),
    }
}

/// Publishes an isolated fork head through `receive-pack`: incoming objects stay
/// quarantined until the exact expected pull ref is accepted. Retrying an
/// already-published head is idempotent.
pub async fn publish_pull_head(
    path: &Path,
    number: i32,
    head: &str,
    expected_head: Option<&str>,
    prepared: &PreparedPullHead,
) -> Result<()> {
    validate_merge_revision("main", head)?;
    if let Some(expected) = expected_head {
        validate_merge_revision("main", expected)?;
    }
    ensure!(prepared.head == head, "Prepared pull request head changed");
    let reference = format!("refs/pull/{number}/head");
    ensure!(
        is_pull_head_ref(&reference),
        "Invalid pull request reference"
    );
    publish_prepared_ref(path, &reference, head, expected_head, prepared).await
}

pub async fn publish_pull_branch_update(
    path: &Path,
    branch: &str,
    expected: &str,
    prepared: &PreparedPullHead,
) -> Result<()> {
    let reference = crate::branch_ref(branch).ok_or_else(|| anyhow!("Invalid branch"))?;
    publish_prepared_ref(path, &reference, prepared.head(), Some(expected), prepared).await
}

async fn publish_prepared_ref(
    path: &Path,
    reference: &str,
    head: &str,
    expected_head: Option<&str>,
    prepared: &PreparedPullHead,
) -> Result<()> {
    validate_merge_revision("main", head)?;
    if let Some(expected) = expected_head {
        validate_merge_revision("main", expected)?;
    }
    ensure!(prepared.head == head, "Prepared commit changed");
    let current = current_reference(path, reference).await?;
    if current.as_deref() == Some(head) {
        // A retry after an interrupted publication still makes the earlier ref durable.
        return sync_prepared_ref(path, reference).await;
    }
    if current.as_deref() != expected_head {
        return Err(PullHeadConflict.into());
    }

    let source = std::path::absolute(prepared.path())?;
    let source = source
        .to_str()
        .ok_or_else(|| anyhow!("Invalid source storage path"))?;
    // Transfer only objects, not a ref. The shared CAS publisher makes their directory
    // entries durable before exposing the commit; a receive-pack push cannot provide that
    // ordering when its hooks are disabled.
    let (status, _) = command(
        path,
        &[
            "-c",
            "fetch.fsckObjects=true",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            "--",
            source,
            head,
        ],
        b"",
        None,
    )
    .await?;
    ensure!(status.success(), "Git prepared object transfer failed");
    let absent = git2::Oid::ZERO_SHA1.to_string();
    match publish_ref(
        path,
        "publish prepared commit",
        reference,
        Some(head),
        expected_head.unwrap_or(&absent),
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(error) if error.is::<RefConflict>() => {
            if current_reference(path, reference).await?.as_deref() == Some(head) {
                sync_prepared_ref(path, reference).await
            } else {
                Err(PullHeadConflict.into())
            }
        }
        Err(error) => Err(error),
    }
}

/// Repeats the barrier when a previous prepared publication already moved its ref.
async fn sync_prepared_ref(path: &Path, reference: &str) -> Result<()> {
    let names = vec![reference.to_owned()];
    durable::sync_refs(path, names, Instant::now() + COMMAND_TIMEOUT).await
}

pub async fn publish_ref(
    path: &Path,
    verb: &str,
    reference: &str,
    new: Option<&str>,
    old: &str,
) -> Result<()> {
    ensure!(
        crate::is_canonical_ref(reference)
            || is_pull_head_ref(reference)
            || is_retained_object_ref(reference),
        "Invalid reference"
    );
    validate_merge_revision("main", old)?;
    if let Some(new) = new {
        validate_merge_revision("main", new)?;
    }
    let input = match new {
        Some(new) => {
            format!("start\noption no-deref\nupdate {reference} {new} {old}\nprepare\ncommit\n")
        }
        None => format!("start\noption no-deref\ndelete {reference} {old}\nprepare\ncommit\n"),
    };
    run_ref_transaction(path, &input, verb, "Reference changed while publishing").await
}

pub async fn publish_branch_rename(path: &Path, old: &str, new: &str, target: &str) -> Result<()> {
    let old_ref = crate::branch_ref(old).ok_or_else(|| anyhow!("Invalid branch"))?;
    let new_ref = crate::branch_ref(new).ok_or_else(|| anyhow!("Invalid branch"))?;
    validate_merge_revision("main", target)?;
    // The caller holds the publication lease. Native rename handles file/directory ref
    // transitions (feature <-> feature/topic), which update-ref cannot lock together.
    if current_reference(path, &old_ref).await?.as_deref() != Some(target)
        || current_reference(path, &new_ref).await?.is_some()
    {
        return Err(RefConflict("Reference changed while renaming".into()).into());
    }
    let outcome = run_ref_command(
        path,
        &["branch", "--move", "--", old, new],
        b"",
        "Reference changed while renaming",
        &[(&old_ref, Some(target)), (&new_ref, None)],
    )
    .await;
    if outcome.is_err() {
        // A timeout or failed exit may still follow an accepted ref move; report it as done so
        // the caller finishes the metadata. Prefix-related refs cannot coexist, so do not probe
        // the old name through the new ref's file.
        let prefix_related = old_ref.starts_with(&format!("{new_ref}/"))
            || new_ref.starts_with(&format!("{old_ref}/"));
        if current_reference(path, &new_ref).await?.as_deref() == Some(target)
            && (prefix_related || current_reference(path, &old_ref).await?.is_none())
        {
            // The move may have committed before the timeout, and its barrier then never
            // ran. Both names matter: one is now a file and the other a directory entry.
            durable::sync_refs(
                path,
                vec![old_ref.clone(), new_ref.clone()],
                Instant::now() + COMMAND_TIMEOUT,
            )
            .await?;
            return Ok(());
        }
    }
    outcome
}

pub async fn publish_ref_create(path: &Path, reference: &str, target: &str) -> Result<()> {
    publish_ref(
        path,
        "create reference",
        reference,
        Some(target),
        "0000000000000000000000000000000000000000",
    )
    .await
}

pub async fn publish_ref_delete(path: &Path, reference: &str, old: &str) -> Result<()> {
    publish_ref(path, "delete reference", reference, None, old).await
}

pub fn merge_head_ref(head_branch: &str, head: &str) -> Result<String> {
    if head_branch.starts_with("refs/heads/") || is_pull_head_ref(head_branch) {
        ensure!(
            git2::Reference::is_valid_name(head_branch),
            "Invalid pull ref"
        );
        validate_merge_revision("main", head)?;
        Ok(head_branch.to_owned())
    } else {
        validate_merge_revision(head_branch, head)?;
        crate::branch_ref(head_branch).ok_or_else(|| anyhow!("Invalid head branch"))
    }
}

/// Moves the base branch to `result` in one transaction, failing if the base or any head moved
/// since the merge was prepared. Each head is `(head branch or pull ref, commit)`.
pub async fn publish_merge(
    path: &Path,
    heads: &[(&str, &str)],
    base_branch: &str,
    base: &str,
    result: &str,
) -> Result<()> {
    validate_merge_revision(base_branch, base)?;
    validate_merge_revision(base_branch, result)?;
    let base_ref = crate::branch_ref(base_branch).ok_or_else(|| anyhow!("Invalid base branch"))?;
    let mut input = String::from("start\n");
    for (head_branch, head) in heads {
        let head_ref = merge_head_ref(head_branch, head)?;
        if head_ref == base_ref {
            bail!("Head and base must be distinct branches");
        }
        input.push_str(&format!("verify {head_ref} {head}\n"));
    }
    input.push_str(&format!(
        "update {base_ref} {result} {base}\nprepare\ncommit\n"
    ));
    run_ref_transaction(
        path,
        &input,
        "merge pull request",
        "Merge failed: a branch changed since the merge was prepared",
    )
    .await
}

/// Moves each branch in `moves` from its old commit to its new one, provided `base_branch` is still
/// at `base`. Each move is `(branch, old, new)` and fails on its own when the branch is not at `old`.
/// A move with `old` equal to `new` only verifies that branch.
/// All of them land in one ref transaction, so either every branch moves or none does.
pub async fn publish_restack(
    path: &Path,
    base_branch: &str,
    base: &str,
    moves: &[(&str, &str, &str)],
) -> Result<()> {
    validate_merge_revision(base_branch, base)?;
    let base_ref = crate::branch_ref(base_branch).ok_or_else(|| anyhow!("Invalid base branch"))?;
    ensure!(!moves.is_empty(), "Nothing to restack");
    let mut input = format!("start\nverify {base_ref} {base}\n");
    for (branch, old, new) in moves {
        ensure!(
            *branch != base_branch,
            "A restack cannot move its base branch"
        );
        validate_merge_revision(branch, old)?;
        validate_merge_revision(branch, new)?;
        let reference = crate::branch_ref(branch).ok_or_else(|| anyhow!("Invalid head branch"))?;
        // A branch the rebase built on but did not rewrite must still be where it was read.
        if old == new {
            input.push_str(&format!("verify {reference} {old}\n"));
        } else {
            input.push_str(&format!("update {reference} {new} {old}\n"));
        }
    }
    input.push_str("prepare\ncommit\n");
    run_ref_transaction(
        path,
        &input,
        "rebase stack",
        "Rebase failed: a branch changed since the rebase was prepared",
    )
    .await
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RefConflict(pub String);

/// Runs one `update-ref --stdin` transaction. A failed check of any line refuses all of them.
pub(crate) async fn run_ref_transaction(
    path: &Path,
    input: &str,
    message: &str,
    failure: &str,
) -> Result<()> {
    let mut expected = Vec::new();
    for line in input.lines() {
        let fields = line.split(' ').collect::<Vec<_>>();
        let pair = match fields.as_slice() {
            ["create", reference, _] => Some((*reference, None)),
            ["update", reference, _, old]
            | ["delete", reference, old]
            | ["verify", reference, old] => Some((
                *reference,
                if old.bytes().all(|byte| byte == b'0') {
                    None
                } else {
                    Some(*old)
                },
            )),
            _ => None,
        };
        if let Some(pair) = pair {
            expected.push(pair);
        }
    }
    run_ref_command(
        path,
        &["update-ref", "--stdin", "-m", message],
        input.as_bytes(),
        failure,
        &expected,
    )
    .await
}

async fn run_ref_command(
    path: &Path,
    args: &[&str],
    input: &[u8],
    failure: &str,
    expected: &[(&str, Option<&str>)],
) -> Result<()> {
    // The refs this command publishes and syncs afterwards. A multi-ref transaction can
    // expand into more ancestor directories than one barrier holds, so plan the same names
    // before the refs move rather than discovering the limit after the command committed.
    let names = expected
        .iter()
        .map(|(reference, _)| (*reference).to_owned())
        .collect::<Vec<_>>();
    durable::RepositorySync::refs(names.clone())
        .preflight(path, Instant::now() + COMMAND_TIMEOUT)
        .await?;
    // A ref must never be published before the object entries that hold its target. The
    // objects exist by now (a merge commit, a fetched pack, an imported write); make their
    // entries durable first, then write the refs, then sync the ref entries themselves.
    durable::RepositorySync::new()
        .objects()
        .sync(path, Instant::now() + COMMAND_TIMEOUT)
        .await?;
    let mut command = process::git_command()?;
    command.arg("--git-dir").arg(path).args(args);
    let output = process::run(command, input, MAX_OUTPUT, COMMAND_TIMEOUT).await?;
    if !output.status.success() {
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        if diagnostics.contains("but expected")
            || diagnostics.contains("unable to resolve reference")
            || diagnostics.contains("reference already exists")
        {
            return Err(RefConflict(failure.into()).into());
        }
        // Preserve storage failures, but classify lease failures even when Git's wording
        // differs between versions or ref backends.
        for (reference, old) in expected {
            if current_reference(path, reference).await?.as_deref() != *old {
                return Err(RefConflict(failure.into()).into());
            }
        }
        bail!("Git ref publication failed: {diagnostics}");
    }
    // Git makes the ref files durable and nothing else: the directory entry that names a
    // new ref, a new nested namespace, a rename and a deleted old name all stay unsynced.
    durable::sync_refs(path, names, Instant::now() + COMMAND_TIMEOUT).await
}

/// Fetches only `sha` and its reachable objects into a new, isolated `destination`
/// and checks it out with a detached HEAD.
pub async fn checkout(path: &Path, destination: &Path, sha: &str) -> Result<()> {
    validate_merge_revision("main", sha)?;
    let source = std::path::absolute(path)?;
    let source = source
        .to_str()
        .ok_or_else(|| anyhow!("Invalid source storage path"))?;
    materialize_checkout(destination, sha, source, None).await
}

pub async fn materialize_checkout(
    destination: &Path,
    sha: &str,
    source: &str,
    pack: Option<tokio::fs::File>,
) -> Result<()> {
    validate_merge_revision("main", sha)?;
    tokio::fs::create_dir(destination).await?;
    let mut init = process::git_command()?;
    init.arg("-C")
        .arg(destination)
        .args(["init", "--template="]);
    let (status, _) = process::buffered(init, b"", MAX_OUTPUT, CHECKOUT_TIMEOUT).await?;
    ensure!(status.success(), "git init failed");
    let mut origin = process::git_command()?;
    origin
        .arg("-C")
        .arg(destination)
        .args(["remote", "add", "--no-tags", "origin", source]);
    let (status, _) = process::buffered(origin, b"", MAX_OUTPUT, CHECKOUT_TIMEOUT).await?;
    ensure!(status.success(), "git remote add failed");
    let mut fetch = process::git_command()?;
    fetch.arg("-C").arg(destination);
    let status = if let Some(pack) = pack {
        fetch.args(["index-pack", "--stdin", "--fix-thin"]);
        process::stream(fetch, pack, tokio::io::sink(), tokio::io::sink(), false).await?
    } else {
        fetch.args(["fetch", "--no-tags", "--", "origin", sha]);
        process::buffered(fetch, b"", MAX_OUTPUT, CHECKOUT_TIMEOUT)
            .await?
            .0
    };
    ensure!(status.success(), "git fetch of CI commit failed");
    let mut switch = process::git_command()?;
    switch
        .arg("-C")
        .arg(destination)
        .args(["checkout", "--detach", "--force", sha]);
    let (status, _) = process::buffered(switch, b"", MAX_OUTPUT, CHECKOUT_TIMEOUT).await?;
    ensure!(status.success(), "git checkout of {sha} failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn head_commit(repo: &git2::Repository, parent: Option<git2::Oid>) -> git2::Oid {
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "test",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn prepared_publication_transfers_objects_without_extra_refs_and_preserves_cas() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.git");
        let base = root.path().join("base.git");
        initialize_repository(&source, None, "main").await.unwrap();
        initialize_repository(&base, None, "main").await.unwrap();
        let source_repo = git2::Repository::open_bare(&source).unwrap();
        let base_repo = git2::Repository::open_bare(&base).unwrap();
        let first = head_commit(&source_repo, None);
        let first_text = first.to_string();
        let prepared = prepare_pull_head(&base, &source, &first_text)
            .await
            .unwrap();
        assert!(base_repo.find_commit(first).is_err());
        publish_pull_head(&base, 7, &first_text, None, &prepared)
            .await
            .unwrap();
        publish_pull_head(&base, 7, &first_text, None, &prepared)
            .await
            .unwrap();
        assert_eq!(base_repo.refname_to_id("refs/pull/7/head").unwrap(), first);
        assert!(base_repo.find_commit(first).is_ok());
        assert_eq!(base_repo.references().unwrap().count(), 1);
        assert!(!base.join("FETCH_HEAD").exists());

        let second = head_commit(&source_repo, Some(first));
        let second_text = second.to_string();
        let prepared = prepare_pull_head(&base, &source, &second_text)
            .await
            .unwrap();
        let error = publish_pull_head(&base, 7, &second_text, None, &prepared)
            .await
            .unwrap_err();
        assert!(error.is::<PullHeadConflict>());
        assert_eq!(base_repo.refname_to_id("refs/pull/7/head").unwrap(), first);
        publish_pull_head(&base, 7, &second_text, Some(&first_text), &prepared)
            .await
            .unwrap();
        publish_pull_head(&base, 7, &second_text, Some(&first_text), &prepared)
            .await
            .unwrap();
        assert_eq!(base_repo.refname_to_id("refs/pull/7/head").unwrap(), second);
        assert_eq!(base_repo.references().unwrap().count(), 1);
        assert!(!base.join("FETCH_HEAD").exists());

        publish_ref(
            &base,
            "seed branch",
            "refs/heads/main",
            Some(&first_text),
            &git2::Oid::ZERO_SHA1.to_string(),
        )
        .await
        .unwrap();
        publish_pull_branch_update(&base, "main", &first_text, &prepared)
            .await
            .unwrap();
        assert_eq!(base_repo.refname_to_id("refs/heads/main").unwrap(), second);
    }

    #[tokio::test]
    async fn initialization_sync_requires_head_and_refuses_symlink_metadata() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repository.git");
        initialize_repository(&path, None, "main").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        std::fs::remove_file(path.join("HEAD")).unwrap();
        assert!(sync_initialized_repository(&path).await.is_err());
        std::os::unix::fs::symlink("config", path.join("HEAD")).unwrap();
        assert!(sync_initialized_repository(&path).await.is_err());
        std::fs::remove_file(path.join("HEAD")).unwrap();
        std::fs::create_dir(path.join("HEAD")).unwrap();
        assert!(sync_initialized_repository(&path).await.is_err());
    }

    #[tokio::test]
    async fn repository_copy_preserves_an_unborn_and_populated_nested_default_branch() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.git");
        initialize_repository(&source, None, "topic/nested")
            .await
            .unwrap();
        copy_repository(&root.path().join("empty.git"), &source, COMMAND_TIMEOUT)
            .await
            .unwrap();
        let repo = git2::Repository::open_bare(&source).unwrap();
        let oid = head_commit(&repo, None);
        repo.tag_lightweight("v1", repo.find_commit(oid).unwrap().as_object(), false)
            .unwrap();
        let destination = root.path().join("populated.git");
        copy_repository(&destination, &source, COMMAND_TIMEOUT)
            .await
            .unwrap();
        let copied = git2::Repository::open_bare(destination).unwrap();
        assert_eq!(
            copied.head().unwrap().name().unwrap(),
            "refs/heads/topic/nested"
        );
        assert_eq!(copied.head().unwrap().target(), Some(oid));
        assert_eq!(copied.refname_to_id("refs/tags/v1").unwrap(), oid);
    }

    #[tokio::test]
    async fn branch_rename_into_and_out_of_a_namespace_syncs_the_replaced_names() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repository.git");
        initialize_repository(&path, None, "main").await.unwrap();
        let repo = git2::Repository::open_bare(&path).unwrap();
        let target = head_commit(&repo, None).to_string();
        publish_branch_rename(&path, "main", "topic/nested", &target)
            .await
            .unwrap();
        assert_eq!(
            current_branch_head(&path, "topic/nested").await.unwrap(),
            Some(target.clone())
        );
        assert!(current_branch_head(&path, "main").await.unwrap().is_none());
        // The reverse move replaces the nested namespace and leaves `main` as a file beside
        // it; both directory entries need syncing.
        publish_branch_rename(&path, "topic/nested", "main", &target)
            .await
            .unwrap();
        assert_eq!(
            current_branch_head(&path, "main").await.unwrap(),
            Some(target)
        );
    }

    #[tokio::test]
    async fn native_multi_ref_publication_refuses_an_expanded_budget_before_git_runs() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repository.git");
        initialize_repository(&path, None, "main").await.unwrap();
        // A restack or merge batch can name many byte-legal refs whose ancestors do not fit
        // one barrier. The plan is refused before update-ref runs, so no ref moves and no
        // directory entry is created.
        let names = (0..256)
            .map(|index| {
                let mut name = format!("refs/heads/{index:02x}/");
                while name.len() + 3 <= crate::ref_names::MAX_REF_BYTES {
                    name.push_str("x/");
                }
                name.push('y');
                name
            })
            .collect::<Vec<_>>();
        let expected = names
            .iter()
            .map(|name| (name.as_str(), None))
            .collect::<Vec<(&str, Option<&str>)>>();
        let error = run_ref_command(
            &path,
            &["update-ref", "--stdin"],
            b"",
            "reference changed",
            &expected,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Too many repository directories"),
            "{error}"
        );
        assert!(!path.join("refs/heads/00").exists());
    }

    #[tokio::test]
    async fn quarantined_ancestry_reads_a_repository_path_containing_a_colon() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a:b").join("repo.git");
        std::fs::create_dir_all(&path).unwrap();
        let repo = git2::Repository::init_bare(&path).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let old = repo
            .commit(None, &signature, &signature, "old", &tree, &[])
            .unwrap()
            .to_string();

        // `new` is only in the quarantine, as for a push that is not accepted yet.
        let quarantine = root.path().join("quarantine");
        std::fs::create_dir(&quarantine).unwrap();
        let who = "Test <test@example.invalid> 1700000001 +0000";
        let body =
            format!("tree {EMPTY_TREE}\nparent {old}\nauthor {who}\ncommitter {who}\n\nnew\n");
        let mut command = process::git_command().unwrap();
        command
            .arg("--git-dir")
            .arg(&path)
            .env("GIT_OBJECT_DIRECTORY", &quarantine)
            .args(["hash-object", "-t", "commit", "-w", "--stdin"]);
        let written = process::buffered(command, body.as_bytes(), MAX_OUTPUT, COMMAND_TIMEOUT);
        let (status, output) = written.await.unwrap();
        assert!(status.success());
        let new = oid_output(&output).unwrap();

        let fast_forward = quarantined_is_fast_forward(&path, &quarantine, &old, &new).await;
        assert!(fast_forward.unwrap());
        let rewind = quarantined_is_fast_forward(&path, &quarantine, &new, &old).await;
        assert!(!rewind.unwrap());
    }
}
