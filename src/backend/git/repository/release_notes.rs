//! A bounded, primary-pinned history for release-note generation. Only object IDs cross RPC.
use super::*;

pub const MAX_RELEASE_NOTE_COMMITS: usize = 10_000;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseHistory {
    pub previous_tag: Option<String>,
    pub commits: Vec<String>,
}

pub(crate) async fn release_history(
    path: &Path,
    head: &str,
    previous: Option<&str>,
    exclude_tag: &str,
) -> Result<ReleaseHistory> {
    let head = {
        let repo = Repository::open_bare(path)?;
        let id = peel_to_commit_checked(&repo, head)?.id().to_string();
        id
    };
    let previous_tag = if let Some(previous) = previous {
        Some(previous.to_owned())
    } else {
        let mut command = process::git_command()?;
        command.current_dir(path).args([
            "describe",
            "--tags",
            "--abbrev=0",
            "--always",
            &format!("--exclude={exclude_tag}"),
            &head,
        ]);
        let (status, output) = process::buffered(command, b"", 4096, COMMIT_WALK_TIMEOUT).await?;
        ensure!(status.success(), "Git could not select the previous tag");
        let name = std::str::from_utf8(&output)?.trim();
        let repo = Repository::open_bare(path)?;
        let tag = repo
            .find_reference(&format!("refs/tags/{name}"))
            .ok()
            .map(|_| name.to_owned());
        tag
    };
    let range = {
        let repo = Repository::open_bare(path)?;
        if let Some(tag) = &previous_tag {
            let base = peel_to_commit_checked(&repo, &format!("refs/tags/{tag}"))?.id();
            format!("{base}..{head}")
        } else {
            head
        }
    };
    let mut command = process::git_command()?;
    command.current_dir(path).args([
        "rev-list",
        &format!("--max-count={}", MAX_RELEASE_NOTE_COMMITS + 1),
        &range,
        "--",
    ]);
    let (status, output) = process::buffered(
        command,
        b"",
        (MAX_RELEASE_NOTE_COMMITS + 1) * 41,
        COMMIT_WALK_TIMEOUT,
    )
    .await?;
    ensure!(status.success(), "Git could not read release history");
    let commits: Vec<String> = std::str::from_utf8(&output)?
        .lines()
        .map(str::to_owned)
        .collect();
    ensure!(
        commits.len() <= MAX_RELEASE_NOTE_COMMITS,
        "Release notes span too many commits; choose a closer previous tag"
    );
    ensure!(
        commits
            .iter()
            .all(|sha| sha.len() == 40 && sha.bytes().all(|ch| ch.is_ascii_hexdigit())),
        "Git returned invalid commit IDs"
    );
    Ok(ReleaseHistory {
        previous_tag,
        commits,
    })
}
