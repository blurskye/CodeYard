//! `codeyard repo verify [--repair]` and `codeyard repo rebuild`.
//!
//! The database may be older than Git, never newer. After a restore, Git can hold pushes and
//! pull request updates the database does not know about, and a repository deleted after the
//! backup sits in `deleted/`. Verify finds these and, with `--repair`, brings the database and
//! the pull request refs back in line with Git. Problems it cannot repair, such as a commit the
//! database names that Git lacks, are listed and make the command fail.
use crate::backend::{db, git::mirror, repositories};
use anyhow::{ensure, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Repository checks read only the repositories this node stores. In cluster mode most of them
/// live on other nodes, so the checks refuse to report success for repositories they did not read.
async fn refuse_cluster(pool: &db::DbPool, command: &str) -> Result<()> {
    ensure!(
        !db::cluster::cluster_enabled(pool).await?,
        "codeyard repo {command} covers a single-node installation; this one is in cluster mode"
    );
    Ok(())
}

/// The pull request number a `refs/pull/<number>/...` ref belongs to.
fn pull_number(reference: &str) -> Option<i64> {
    reference
        .strip_prefix("refs/pull/")?
        .split_once('/')?
        .0
        .parse()
        .ok()
}

/// Checks every repository this node stores and returns how many problems remain.
pub async fn verify(pool: &db::DbPool, repair: bool) -> Result<usize> {
    refuse_cluster(pool, "verify").await?;
    let mut problems = 0;
    let mut report = |line: String| {
        println!("{line}");
        problems += 1;
    };
    for repository in db::backups::list_repositories(pool).await? {
        if repository.storage_node_id.is_some() {
            continue;
        }
        let label = format!("{} (id {})", repository.name, repository.id);
        let path = Path::new(&repository.disk_path);
        if !path.join("HEAD").is_file() {
            let root = path.parent().context("a repository path has no parent")?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("a repository path has no name")?;
            match mirror::deleted_copy(root, name)? {
                Some(kept) if repair => {
                    std::fs::rename(&kept, path)?;
                    std::fs::File::open(root)?.sync_all()?;
                    println!("{label}: restored from {}", kept.display());
                }
                Some(kept) => {
                    report(format!(
                        "{label}: missing; a deleted copy is kept at {}",
                        kept.display()
                    ));
                    continue;
                }
                None => {
                    report(format!("{label}: the repository files are missing"));
                    continue;
                }
            }
        }
        if let Some(fsck) = mirror::check(path).await? {
            report(format!("{label}: git fsck: {fsck}"));
        }
        // A default branch changed after the backup moved HEAD in Git, which is newer.
        if let Some(head) = mirror::head_branch(path).await? {
            if head != repository.default_branch {
                if !repair {
                    report(format!(
                        "{label}: the default branch is recorded as {}, Git HEAD names {head}",
                        repository.default_branch
                    ));
                } else if db::repositories::records::restore_repository_default_branch(
                    pool,
                    repository.id,
                    &repository.default_branch,
                    &head,
                )
                .await?
                {
                    println!("{label}: the default branch now records {head}");
                } else {
                    report(format!("{label}: the default branch changed meanwhile"));
                }
            }
        }

        let refs: HashMap<String, String> = mirror::refs(path, "refs/pull/")
            .await?
            .into_iter()
            .collect();
        let mut known = HashSet::new();
        let mut updates = String::new();
        for pull in db::backups::list_pull_request_heads(pool, repository.id).await? {
            known.insert(pull.number);
            let number = pull.number;
            let name = format!("refs/pull/{number}/head");
            match refs.get(&name) {
                Some(sha) if *sha == pull.head_sha => {}
                // A push after the backup moved the pull request; Git is newer.
                Some(sha) if pull.state == "open" && mirror::has_commit(path, sha).await? => {
                    if !repair {
                        report(format!(
                            "{label}: pull request #{number} records {}, Git has {sha}",
                            pull.head_sha
                        ));
                    } else if db::pull_requests::records::move_open_pull_request_head(
                        pool,
                        pull.id,
                        &pull.head_sha,
                        sha,
                        pull.head_repo_id,
                        &pull.head_branch,
                    )
                    .await?
                    {
                        println!("{label}: pull request #{number} now records {sha}");
                    } else {
                        report(format!("{label}: pull request #{number} changed meanwhile"));
                    }
                }
                // A pull request reopened after the backup moved its head in Git. A closed row
                // keeps the head it recorded, so the ref goes back to that commit.
                Some(sha)
                    if repair
                        && pull.state != "open"
                        && mirror::has_commit(path, &pull.head_sha).await? =>
                {
                    updates.push_str(&format!("update {name} {} {sha}\n", pull.head_sha));
                    println!(
                        "{label}: pull request #{number} ({}) reset {name} to {}",
                        pull.state, pull.head_sha
                    );
                }
                Some(sha) => report(format!(
                    "{label}: pull request #{number} ({}) records {}, Git has {sha}",
                    pull.state, pull.head_sha
                )),
                None if mirror::has_commit(path, &pull.head_sha).await? => {
                    if repair {
                        updates.push_str(&format!("create {name} {}\n", pull.head_sha));
                        println!("{label}: recreated {name}");
                    } else {
                        report(format!("{label}: {name} is missing"));
                    }
                }
                None => report(format!(
                    "{label}: pull request #{number} head {} is missing from Git",
                    pull.head_sha
                )),
            }
            // Nothing keeps a closed pull request's base in Git, so once its branch moves on the
            // commit may be pruned. Only an open pull request still needs it.
            let bases = std::iter::once(&pull.base_sha).chain(&pull.fork_sha);
            for base in bases.filter(|_| pull.state == "open") {
                if !mirror::has_commit(path, base).await? {
                    report(format!(
                        "{label}: pull request #{number} base commit {base} is missing from Git"
                    ));
                }
            }
            if let Some(merge) = &pull.merge_commit_sha {
                if !mirror::has_commit(path, merge).await? {
                    report(format!(
                        "{label}: pull request #{number} merge commit {merge} is missing from Git"
                    ));
                }
            }
        }
        // Pull requests opened after the backup have refs but no rows. Their numbers will be
        // given out again, so the refs must go; the commits stay until Git prunes them.
        for name in refs.keys() {
            if pull_number(name).is_some_and(|number| !known.contains(&number)) {
                if repair {
                    updates.push_str(&format!("delete {name}\n"));
                    println!("{label}: removed {name}, which names no pull request");
                } else {
                    report(format!("{label}: {name} names no pull request"));
                }
            }
        }
        if !updates.is_empty() {
            mirror::update_refs(path, &updates).await?;
        }
        // A run that has not finished still checks its commit out. A finished run's commit may
        // be pruned like any other unreachable commit.
        for sha in db::backups::list_ci_revisions(pool, repository.id).await? {
            if !mirror::has_commit(path, &sha).await? {
                report(format!(
                    "{label}: an unfinished CI run's commit {sha} is missing from Git"
                ));
            }
        }
    }
    if problems == 0 {
        println!("No problems found.");
    }
    Ok(problems)
}

/// Rebuilds what is derived from the stored repositories: Git's pack indexes and commit
/// graph, then each default branch's language statistics.
pub async fn rebuild(pool: &db::DbPool) -> Result<()> {
    refuse_cluster(pool, "rebuild").await?;
    let mut rebuilt = 0;
    for repository in db::backups::list_repositories(pool).await? {
        if repository.storage_node_id.is_some() {
            continue;
        }
        let path = Path::new(&repository.disk_path);
        if !path.join("HEAD").is_file() {
            continue;
        }
        mirror::rebuild(path)
            .await
            .with_context(|| format!("rebuild {} (id {})", repository.name, repository.id))?;
        // The language rows are derived from Git. Forget the cache marker first, or the scan
        // returns the rows that were lost.
        db::repository_stats::forget_languages(pool, repository.id).await?;
        let store = repositories::store::repo_store(&repository, None);
        repositories::languages::compute_and_store(
            pool,
            &store,
            &repository,
            &repository.default_branch,
        )
        .await
        .map_err(|error| anyhow::anyhow!("languages of {}: {error}", repository.name))?;
        rebuilt += 1;
    }
    println!("Rebuilt {rebuilt} repositories.");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn pull_refs_name_their_number() {
        assert_eq!(super::pull_number("refs/pull/12/head"), Some(12));
        assert_eq!(super::pull_number("refs/pull/3/merge"), Some(3));
        assert_eq!(super::pull_number("refs/pull/x/head"), None);
        assert_eq!(super::pull_number("refs/heads/main"), None);
    }
}
