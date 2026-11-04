//! `codeyard repo verify [--repair]` and `codeyard repo rebuild`.
//!
//! The database may be older than Git, never newer. After a restore, Git can hold pushes and
//! pull request updates the database does not know about, and a repository deleted after the
//! backup sits in `deleted/`. Verify finds these and, with `--repair`, brings the database and
//! the pull request refs back in line with Git. Problems it cannot repair, such as a commit the
//! database names that Git lacks, are listed and make the command fail.

use anyhow::ensure;

use anyhow::Result;
use codeyard_git as git;
use codeyard_git::mirror;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// Repository checks read only the repositories this node stores. In cluster mode most of them
/// live on other nodes, so the checks refuse to report success for repositories they did not read.
async fn refuse_cluster(pool: &codeyard_db_core::DbPool, command: &str) -> Result<()> {
    ensure!(
        !codeyard_db_cluster::cluster::cluster_enabled(pool).await?,
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

/// A retained pull ref has no branch provenance. Hold the recorded head branch's publication
/// lease through the SQL CAS, and accept its retained commit only while that live branch matches.
async fn retain_matching_head(
    pool: &codeyard_db_core::DbPool,
    pull: &codeyard_db_cluster::backups::PullRequestHead,
    candidate: &str,
) -> Result<Option<Arc<git::locks::PublicationGuard>>> {
    let Some(head_id) = pull.head_repo_id else {
        return Ok(None);
    };
    let Some(head) =
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, head_id).await?
    else {
        return Ok(None);
    };
    if head.storage_node_id.is_some() || head.is_gist {
        return Ok(None);
    }
    let Some(reference) = git::branch_ref(&pull.head_branch) else {
        return Ok(None);
    };
    let path = Path::new(&head.disk_path);
    if !path.join("HEAD").is_file() {
        return Ok(None);
    }
    let held = git::locks::publication(path).await?;
    let matches = mirror::refs(path, &reference)
        .await?
        .into_iter()
        .any(|(name, sha)| name == reference && sha == candidate);
    Ok(matches.then_some(held))
}

/// Checks every repository this node stores and returns how many problems remain.
pub async fn verify(pool: &codeyard_db_core::DbPool, repair: bool) -> Result<usize> {
    refuse_cluster(pool, "verify").await?;
    let mut problems = 0;
    let mut report = |line: String| {
        println!("{line}");
        problems += 1;
    };
    let mut deleted = HashMap::new();
    for repository in codeyard_db_cluster::backups::list_repositories(pool).await? {
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
            let copies = match deleted.entry(root.to_owned()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(mirror::deleted_copies(root)?)
                }
            };
            match copies.get(name).cloned() {
                Some(kept) if repair => {
                    std::fs::rename(&kept, path)?;
                    copies.remove(name);
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
        // A default branch changed after the backup moved HEAD in Git, which is newer.
        if let Some(head) = mirror::head_branch(path).await? {
            if head != repository.default_branch {
                if !repair {
                    report(format!(
                        "{label}: the default branch is recorded as {}, Git HEAD names {head}",
                        repository.default_branch
                    ));
                } else if codeyard_db_repos::repositories::records::restore_repository_default_branch(
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
        let pulls =
            codeyard_db_cluster::backups::list_pull_request_heads(pool, repository.id).await?;
        let ci_revisions =
            codeyard_db_cluster::backups::list_ci_revisions(pool, repository.id).await?;
        let branches: HashMap<String, String> = mirror::refs(path, "refs/heads/")
            .await?
            .into_iter()
            .collect();
        let mut revisions = ci_revisions.clone();
        revisions.extend(refs.values().cloned());
        revisions.extend(branches.values().cloned());
        for pull in &pulls {
            revisions.extend([pull.head_sha.clone(), pull.base_sha.clone()]);
            revisions.extend(pull.fork_sha.iter().chain(&pull.merge_commit_sha).cloned());
        }
        revisions.sort();
        revisions.dedup();
        let present = mirror::existing_commits(path, &revisions).await?;
        for mut pull in pulls {
            known.insert(pull.number);
            let number = pull.number;
            let name = format!("refs/pull/{number}/head");
            match refs.get(&name) {
                Some(sha) if *sha == pull.head_sha => {}
                // Git may be newer, but the retained ref alone does not identify its source.
                Some(sha) if pull.state == "open" && present.contains(sha) => {
                    if let Some(_head) = retain_matching_head(pool, &pull, sha).await? {
                        if !repair {
                            report(format!(
                                "{label}: pull request #{number} records {}, its live head branch has {sha}",
                                pull.head_sha
                            ));
                        } else if codeyard_db_repos::pull_requests::records::move_open_pull_request_head(
                            pool,
                            pull.id,
                            &pull.head_sha,
                            sha,
                            pull.head_repo_id,
                            &pull.head_branch,
                            None,
                        )
                        .await?
                        {
                            println!("{label}: pull request #{number} now records {sha}");
                        } else {
                            report(format!("{label}: pull request #{number} changed meanwhile"));
                        }
                    } else {
                        report(format!(
                            "{label}: pull request #{number} retains {sha}, which cannot be verified against its recorded head repository and branch"
                        ));
                    }
                }
                // A pull request reopened after the backup moved its head in Git. A closed row
                // keeps the head it recorded, so the ref goes back to that commit.
                Some(sha) if repair && pull.state != "open" && present.contains(&pull.head_sha) => {
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
                None if present.contains(&pull.head_sha) => {
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
            if pull.state == "open" {
                if let Some(base) = branches
                    .get(&format!("refs/heads/{}", pull.base_branch))
                    .filter(|base| **base != pull.base_sha)
                {
                    if !repair {
                        report(format!(
                            "{label}: pull request #{number} records base {}, Git has {base}",
                            pull.base_sha
                        ));
                    } else if codeyard_db_repos::pull_requests::records::move_open_pull_request_base(
                        pool,
                        pull.id,
                        &pull.base_sha,
                        base,
                        &pull.base_branch,
                        pull.fork_sha.as_deref(),
                    )
                    .await?
                    {
                        pull.base_sha = base.clone();
                        println!("{label}: pull request #{number} now records base {base}");
                    } else {
                        report(format!(
                            "{label}: pull request #{number} base changed meanwhile"
                        ));
                    }
                }
            }
            // Closed discussions and review diffs still require their recorded bases.
            let bases = std::iter::once(&pull.base_sha).chain(&pull.fork_sha);
            for base in bases {
                if !present.contains(base) {
                    report(format!(
                        "{label}: pull request #{number} base commit {base} is missing from Git"
                    ));
                }
            }
            if let Some(merge) = &pull.merge_commit_sha {
                if !present.contains(merge) {
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
        for sha in ci_revisions {
            if !present.contains(&sha) {
                report(format!(
                    "{label}: an unfinished CI run's commit {sha} is missing from Git"
                ));
            }
        }
        if let Some(fsck) = mirror::check(path).await? {
            report(format!("{label}: git fsck: {fsck}"));
        }
    }
    if problems == 0 {
        println!("No problems found.");
    }
    Ok(problems)
}

/// Rebuilds what is derived from the stored repositories: Git's pack indexes and commit
/// graph, then each default branch's language statistics.
pub async fn rebuild(pool: &codeyard_db_core::DbPool) -> Result<()> {
    refuse_cluster(pool, "rebuild").await?;
    let mut rebuilt = 0;
    for repository in codeyard_db_cluster::backups::list_repositories(pool).await? {
        if repository.storage_node_id.is_some() {
            continue;
        }
        codeyard_db_repos::repositories::records::advance_contribution_version(pool, repository.id)
            .await?;
        let path = Path::new(&repository.disk_path);
        if !path.join("HEAD").is_file() {
            continue;
        }
        mirror::rebuild(path)
            .await
            .with_context(|| format!("rebuild {} (id {})", repository.name, repository.id))?;
        // The language rows are derived from Git. Forget the cache marker first, or the scan
        // returns the rows that were lost.
        codeyard_db_repos::repository_stats::forget_languages(pool, repository.id).await?;
        let store = crate::repositories::store::repo_store(&repository, None);
        crate::repositories::languages::compute_and_store(
            pool,
            &store,
            &repository,
            &repository.default_branch,
        )
        .await
        .map_err(|error| anyhow::anyhow!("languages of {}: {error}", repository.name))?;
        let size = i64::try_from(store.size_bytes().await?)?;
        codeyard_db_repos::repositories::records::update_repo_size(pool, repository.id, size)
            .await?;
        rebuilt += 1;
    }
    println!("Rebuilt {rebuilt} repositories.");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn pull_refs_name_their_number() {
        assert_eq!(
            crate::backups::repair::pull_number("refs/pull/12/head"),
            Some(12)
        );
        assert_eq!(
            crate::backups::repair::pull_number("refs/pull/3/merge"),
            Some(3)
        );
        assert_eq!(
            crate::backups::repair::pull_number("refs/pull/x/head"),
            None
        );
        assert_eq!(crate::backups::repair::pull_number("refs/heads/main"), None);
    }
}
