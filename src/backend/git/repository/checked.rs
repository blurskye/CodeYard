//! Merge commits, repository creation and CI checkouts from exact revisions.
//! Ref updates go through `update-ref` transactions that check the old value.
use super::{process, MAX_COMMIT_EMAIL_BYTES, MAX_COMMIT_NAME_BYTES};
use anyhow::{anyhow, bail, ensure, Result};
use std::{path::Path, time::Duration};

const MAX_OUTPUT: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(300);
/// Longest merge or squash message, title included. A squash body is cut to fit.
const MAX_MERGE_MESSAGE_BYTES: usize = 8192;
/// Conflicting paths that a refusal names. The rest are counted.
const LISTED_CONFLICTS: usize = 20;

/// Shown when a merge would add nothing to the base branch.
pub(crate) const NO_CHANGES: &str = "The pull request adds no changes to the base branch.";

/// Someone who writes or commits a commit. An author date keeps the date of the commit the
/// author came from, in Git's `<seconds> <offset>` form.
pub(crate) struct Person<'a> {
    pub(crate) name: &'a str,
    pub(crate) email: &'a str,
    pub(crate) date: Option<&'a str>,
}

/// The branches conflict. The message names the conflicting files.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct MergeConflict(pub(crate) String);

/// The commits cannot be merged this way, for example because they contain merge commits.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct UnsupportedMerge(pub(crate) String);

pub(crate) fn validate_merge_revision(branch: &str, sha: &str) -> Result<()> {
    if crate::backend::git::branch_ref(branch).is_none()
        || sha.len() != 40
        || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
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
    if args.first() == Some(&"merge-tree") {
        process::limit_memory(&mut command);
    }
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
    if text.len() != 40 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Git returned an invalid object ID");
    }
    Ok(text.to_ascii_lowercase())
}

pub(crate) async fn commit_is_ancestor(path: &Path, base: &str, head: &str) -> Result<bool> {
    validate_merge_revision("main", base)?;
    validate_merge_revision("main", head)?;
    let (status, _) = command(
        path,
        &["merge-base", "--is-ancestor", base, head],
        b"",
        None,
    )
    .await?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("Git ancestry check failed"),
    }
}

/// Git splits `GIT_ALTERNATE_OBJECT_DIRECTORIES` on `:`. Quoting the path as one C-style
/// string keeps a repository path that contains `:` a single entry.
fn quote_alternate(objects: &Path) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut quoted = vec![b'"'];
    for &byte in objects.as_os_str().as_bytes() {
        match byte {
            b'\\' | b'"' => quoted.extend_from_slice(&[b'\\', byte]),
            b'\n' => quoted.extend_from_slice(br"\n"),
            _ => quoted.push(byte),
        }
    }
    quoted.push(b'"');
    std::ffi::OsString::from_vec(quoted)
}

/// Runs `git` against `path` with the objects of a push that Git has not yet accepted.
/// Receive-pack keeps them in `quarantine`, which is not part of the repository's own
/// object directory until the push is accepted.
async fn quarantined(
    path: &Path,
    quarantine: &Path,
    args: &[&str],
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut command = process::git_command()?;
    command
        .arg("--git-dir")
        .arg(path)
        .env("GIT_OBJECT_DIRECTORY", quarantine)
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            quote_alternate(&path.join("objects")),
        )
        .args(args);
    process::buffered(command, b"", MAX_OUTPUT, COMMAND_TIMEOUT).await
}

/// Whether moving a branch from `old` to `new` only adds commits, for a push still in
/// quarantine.
pub(crate) async fn quarantined_is_fast_forward(
    path: &Path,
    quarantine: &Path,
    old: &str,
    new: &str,
) -> Result<bool> {
    validate_merge_revision("main", old)?;
    validate_merge_revision("main", new)?;
    let (status, _) =
        quarantined(path, quarantine, &["merge-base", "--is-ancestor", old, new]).await?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("Git ancestry check failed"),
    }
}

/// Whether `new` brings a merge commit that `old` does not have. A new branch checks
/// its whole history, even when another ref already holds it. A push still in
/// `quarantine` is read through it; a prepared merge is already in the repository.
pub(crate) async fn adds_merge_commit(
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

/// Builds a two-parent merge commit of `base` and `head` without touching refs, even when `head`
/// already contains `base`. Conflicts are a [`MergeConflict`].
pub(crate) async fn prepare_merge(
    path: &Path,
    head: &str,
    base: &str,
    message: &str,
    committer: &Person<'_>,
) -> Result<String> {
    validate_merge_revision("main", head)?;
    validate_merge_revision("main", base)?;
    check_commit_text(message, &[committer])?;
    let tree = merge_tree(path, base, head).await?;
    commit_tree(
        path,
        &tree,
        &[base, head],
        message.as_bytes(),
        committer,
        committer,
    )
    .await
}

/// Builds one commit on top of `base` that holds the whole change of `head`. `author` wrote it
/// and `committer` commits it. The message is `title`, then the subject of each commit it
/// squashes, cut to the size limit. A change that leaves the base tree as it is, is refused.
pub(crate) async fn prepare_squash(
    path: &Path,
    head: &str,
    base: &str,
    title: &str,
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
    let message = squash_message(path, base, head, title).await?;
    commit_tree(path, &tree, &[base], message.as_bytes(), author, committer).await
}

/// `title`, then a `* subject` line for each commit in `base..head`, oldest first, for as many
/// as fit in the message limit.
async fn squash_message(path: &Path, base: &str, head: &str, title: &str) -> Result<String> {
    let range = format!("{base}..{head}");
    let _permit = process::operation_slot(COMMAND_TIMEOUT).await?;
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
    let mut child = command.spawn()?;
    let mut group = process::ProcessGroup(child.id());
    let mut reader = tokio::io::BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("git log stdout"))?,
    );
    let mut listed = String::new();
    let read = async {
        while let Some((subject, _)) = super::read_delimited(&mut reader, 0, 100).await? {
            let subject = String::from_utf8_lossy(&subject);
            let mut end = subject.len().min(100);
            while !subject.is_char_boundary(end) {
                end -= 1;
            }
            let line = format!("* {}\n", subject[..end].trim_end_matches(['\0', '\n', ' ']));
            if title.len() + 2 + listed.len() + line.len() > MAX_MERGE_MESSAGE_BYTES {
                return Ok(());
            }
            listed.push_str(&line);
        }
        ensure!(
            child.wait().await?.success(),
            "Git could not list the commits to squash"
        );
        group.0 = None;
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(COMMAND_TIMEOUT, read).await??;
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
    let valid_person = |person: &Person<'_>| {
        person.name.len() <= MAX_COMMIT_NAME_BYTES
            && person.email.len() <= MAX_COMMIT_EMAIL_BYTES
            && !person.name.contains(['\n', '\r', '\0'])
            && !person.email.contains(['\n', '\r', '\0'])
    };
    ensure!(
        !message.is_empty()
            && message.len() <= MAX_MERGE_MESSAGE_BYTES
            && people.iter().all(|person| valid_person(person)),
        "Invalid merge commit identity or message"
    );
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
    let mut args = vec!["commit-tree", tree];
    for parent in parents {
        args.extend(["-p", *parent]);
    }
    let (status, output) = command(path, &args, message, Some((author, committer))).await?;
    ensure!(status.success(), "Git failed to construct the commit");
    oid_output(&output)
}

/// Creates a bare repository. Forks fetch refs into a fresh repository instead
/// of copying the source's config, hooks or alternates.
pub(crate) async fn initialize_repository(
    path: &Path,
    source: Option<&Path>,
    default_branch: &str,
) -> Result<()> {
    validate_merge_revision(default_branch, "0000000000000000000000000000000000000000")?;
    let branch = format!("--initial-branch={default_branch}");
    let (status, _) = command(path, &["init", "--bare", &branch], b"", None).await?;
    if !status.success() {
        bail!("Git repository initialization failed");
    }
    if let Some(source) = source {
        let source = source
            .to_str()
            .ok_or_else(|| anyhow!("Invalid source storage path"))?;
        let (status, _) = command(
            path,
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
        if !status.success() {
            bail!("Git fork snapshot failed");
        }
    }
    Ok(())
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
pub(crate) struct PreparedPullHead {
    temporary: tempfile::TempDir,
    head: String,
}

impl PreparedPullHead {
    fn path(&self) -> &Path {
        self.temporary.path()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Git pull request ref changed")]
pub(crate) struct PullHeadConflict;

/// Fetches a local fork commit into temporary storage. The base repository is
/// not touched until [`publish_pull_head`] runs after the live authority check.
pub(crate) async fn prepare_pull_head(
    base: &Path,
    source: &Path,
    head: &str,
) -> Result<PreparedPullHead> {
    validate_merge_revision("main", head)?;
    let source = std::path::absolute(source)?;
    ensure!(source.is_dir(), "Repository storage is unavailable");
    let source = source
        .to_str()
        .ok_or_else(|| anyhow!("Invalid source storage path"))?;
    let temporary = tempfile::tempdir()?;
    initialize_repository(temporary.path(), None, "main").await?;
    let base = std::fs::canonicalize(base)?;
    let objects = base.join("objects");
    let objects = objects
        .to_str()
        .ok_or_else(|| anyhow!("Invalid base storage path"))?;
    ensure!(!objects.contains(['\n', '\r']), "Invalid base storage path");
    tokio::fs::write(
        temporary.path().join("objects/info/alternates"),
        format!("{objects}\n"),
    )
    .await?;
    let refs = super::list_mirror_refs(&base)?;
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
            source,
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

fn is_pull_head_ref(reference: &str) -> bool {
    reference
        .strip_prefix("refs/pull/")
        .and_then(|tail| tail.strip_suffix("/head"))
        .and_then(|number| number.parse::<i32>().ok())
        .is_some_and(|number| number > 0)
        && git2::Reference::is_valid_name(reference)
}

pub(crate) async fn current_pull_head(path: &Path, number: i32) -> Result<Option<String>> {
    ensure!(number > 0, "Invalid pull request number");
    let reference = format!("refs/pull/{number}/head");
    ensure!(
        is_pull_head_ref(&reference),
        "Invalid pull request reference"
    );
    let (status, current) = command(
        path,
        &["rev-parse", "--verify", "--quiet", &reference],
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
pub(crate) async fn publish_pull_head(
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
    let current = current_pull_head(path, number).await?;
    if current.as_deref() == Some(head) {
        return Ok(());
    }
    if current.as_deref() != expected_head {
        return Err(PullHeadConflict.into());
    }

    let destination = std::path::absolute(path)?;
    ensure!(destination.is_dir(), "Repository storage is unavailable");
    let destination = destination
        .to_str()
        .ok_or_else(|| anyhow!("Invalid destination storage path"))?;
    let lease = format!(
        "--force-with-lease={reference}:{}",
        expected_head.unwrap_or_default()
    );
    let refspec = format!("{TEMPORARY_PULL_HEAD}:{reference}");
    let (status, _) = command(
        prepared.path(),
        &[
            "push",
            "--porcelain",
            "--no-verify",
            &lease,
            "--",
            destination,
            &refspec,
        ],
        b"",
        None,
    )
    .await?;
    if status.success() {
        ensure!(
            current_pull_head(path, number).await?.as_deref() == Some(head),
            "Git pull request publication returned the wrong commit"
        );
        return Ok(());
    }
    let current = current_pull_head(path, number).await?;
    if current.as_deref() == Some(head) {
        return Ok(());
    }
    if current.as_deref() != expected_head {
        return Err(PullHeadConflict.into());
    }
    bail!("Git pull request publication failed")
}

async fn publish_ref(
    path: &Path,
    verb: &str,
    reference: &str,
    new: Option<&str>,
    old: &str,
) -> Result<()> {
    ensure!(
        crate::backend::git::is_canonical_ref(reference) || is_pull_head_ref(reference),
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
    let (status, _) = command(
        path,
        &["update-ref", "--stdin", "-m", verb],
        input.as_bytes(),
        None,
    )
    .await?;
    ensure!(status.success(), "Reference changed while publishing");
    Ok(())
}

pub(crate) async fn publish_ref_create(path: &Path, reference: &str, target: &str) -> Result<()> {
    publish_ref(
        path,
        "create reference",
        reference,
        Some(target),
        "0000000000000000000000000000000000000000",
    )
    .await
}

pub(crate) async fn publish_ref_delete(path: &Path, reference: &str, old: &str) -> Result<()> {
    publish_ref(path, "delete reference", reference, None, old).await
}

/// Moves the base branch to `result` in one transaction, failing if the base or any head moved
/// since the merge was prepared. Each head is `(head branch or pull ref, commit)`.
pub(crate) async fn publish_merge(
    path: &Path,
    heads: &[(&str, &str)],
    base_branch: &str,
    base: &str,
    result: &str,
) -> Result<()> {
    validate_merge_revision(base_branch, base)?;
    validate_merge_revision(base_branch, result)?;
    let base_ref = crate::backend::git::branch_ref(base_branch)
        .ok_or_else(|| anyhow!("Invalid base branch"))?;
    let mut input = String::from("start\n");
    for (head_branch, head) in heads {
        let head_ref = if head_branch.starts_with("refs/heads/") || is_pull_head_ref(head_branch) {
            ensure!(
                git2::Reference::is_valid_name(head_branch),
                "Invalid pull ref"
            );
            validate_merge_revision("main", head)?;
            (*head_branch).to_owned()
        } else {
            validate_merge_revision(head_branch, head)?;
            crate::backend::git::branch_ref(head_branch)
                .ok_or_else(|| anyhow!("Invalid head branch"))?
        };
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
/// All of them land in one ref transaction, so either every branch moves or none does.
pub(crate) async fn publish_restack(
    path: &Path,
    base_branch: &str,
    base: &str,
    moves: &[(&str, &str, &str)],
) -> Result<()> {
    validate_merge_revision(base_branch, base)?;
    let base_ref = crate::backend::git::branch_ref(base_branch)
        .ok_or_else(|| anyhow!("Invalid base branch"))?;
    ensure!(!moves.is_empty(), "Nothing to restack");
    let mut input = format!("start\nverify {base_ref} {base}\n");
    for (branch, old, new) in moves {
        ensure!(
            *branch != base_branch,
            "A restack cannot move its base branch"
        );
        validate_merge_revision(branch, old)?;
        validate_merge_revision(branch, new)?;
        let reference = crate::backend::git::branch_ref(branch)
            .ok_or_else(|| anyhow!("Invalid head branch"))?;
        input.push_str(&format!("update {reference} {new} {old}\n"));
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
pub(crate) struct RefConflict(pub(crate) String);

/// Runs one `update-ref --stdin` transaction. A failed check of any line refuses all of them.
async fn run_ref_transaction(path: &Path, input: &str, message: &str, failure: &str) -> Result<()> {
    let mut command = process::git_command()?;
    command
        .arg("--git-dir")
        .arg(path)
        .args(["update-ref", "--stdin", "-m", message]);
    let output = process::run(command, input.as_bytes(), MAX_OUTPUT, COMMAND_TIMEOUT).await?;
    if !output.status.success() {
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        if diagnostics.contains("but expected")
            || diagnostics.contains("unable to resolve reference")
            || diagnostics.contains("reference already exists")
        {
            return Err(RefConflict(failure.into()).into());
        }
        bail!("Git ref transaction failed: {diagnostics}");
    }
    Ok(())
}

/// Fetches only `sha` and its reachable objects into a new, isolated `destination`
/// and checks it out with a detached HEAD.
pub(crate) async fn checkout(path: &Path, destination: &Path, sha: &str) -> Result<()> {
    validate_merge_revision("main", sha)?;
    let source = std::path::absolute(path)?;
    let source = source
        .to_str()
        .ok_or_else(|| anyhow!("Invalid source storage path"))?;
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
    fetch
        .arg("-C")
        .arg(destination)
        .args(["fetch", "--no-tags", "--", "origin", sha]);
    let (status, _) = process::buffered(fetch, b"", MAX_OUTPUT, CHECKOUT_TIMEOUT).await?;
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
