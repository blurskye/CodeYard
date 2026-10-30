//! Verified proposals only. No package manager, repository script, force push or auto-merge.
use super::{npm, registry::Client, worker::Pinned};
use crate::backend::{
    ci::CiRuns,
    db::{
        self,
        repositories::dependencies::{
            self as records, AlertState, Feature, Job, Publication, Refusal, Update,
        },
    },
    notifications::NotificationBroadcaster,
    repositories::{
        pull_requests::{
            self,
            mutations::{create_pull_request, CreatePullRequestRequest},
        },
        store::{repo_store, UploadFile, UploadWrite},
    },
};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};

/// The owner name of a repository, as push events and pull requests name it.
async fn owner_name(pool: &db::DbPool, repository: i64) -> Result<String> {
    db::repositories::records::repository_owner_names(pool, &[repository])
        .await?
        .get(&repository)
        .cloned()
        .context("Repository owner unavailable")
}

async fn open(
    pool: &db::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    job: &Job,
    pinned: &Pinned,
    key: &str,
    update: &Update,
) -> Result<()> {
    records::check_job(pool, job, Feature::Updates).await?;
    let actor = job.updates_actor_id.ok_or(Refusal::Denied)?;
    let snapshot = records::snapshot(pool, actor, pinned.repository.id).await?;
    let alert = snapshot.alerts.get(key).ok_or(Refusal::Missing)?;
    if alert.state != AlertState::Open || alert.finding.package.version != update.old_version {
        return Ok(());
    }
    let existing =
        records::proposal_number(pool, actor, pinned.repository.id, &update.branch).await?;
    let pull = if let Some(number) = existing {
        // Include closed/merged proposals: a crash before receipt storage never reopens one.
        db::pull_requests::records::get_pull_request(pool, pinned.repository.id, number)
            .await?
            .ok_or(Refusal::Stale)?
    } else {
        let refs = repo_store(&pinned.repository, ci.cluster.as_ref())
            .refs_from_primary()
            .await?;
        ensure!(
            refs.get(&format!("refs/heads/{}", update.branch)) == Some(&update.commit_sha),
            Refusal::Stale
        );
        let owner = owner_name(pool, pinned.repository.id).await?;
        let title = format!(
            "Address {} in {}",
            alert.finding.advisory_id, alert.finding.package.name
        )
        .chars()
        .take(240)
        .collect();
        let body=format!("Proposes a verified registry patch for `{}`: `{}` → `{}`.\n\nAdvisory: [`{}`]({}).\n\nPrepared from commit `{}`. Registry SHA-512 identity, package metadata and the unchanged dependency/platform/license graph were checked; The local synchronized advisory catalogue reported no known vulnerabilities for the proposed package version when prepared.\n\nNo package manager or repository scripts were run by the updater. This is not an independent code audit. Normal branch rules apply. Automatic proposals do not run workflows; a trusted user must explicitly run them. Review before merging; this service never auto-merges.",alert.finding.package.name,update.old_version,update.fixed_version,alert.finding.advisory_id,alert.finding.url,update.base_sha);
        match create_pull_request(
            pool,
            ci,
            broadcaster,
            actor,
            owner,
            pinned.repository.name.clone(),
            CreatePullRequestRequest {
                suppress_ci: true,
                head_repo_id: Some(pinned.repository.id),
                source: pull_requests::mutations::PullRequestSource::New {
                    title,
                    body: Some(body),
                },
                maintainer_can_modify: None,
                head_branch: update.branch.clone(),
                base_branch: pinned.repository.default_branch.clone(),
                is_draft: false,
            },
        )
        .await
        {
            Ok(pull) => pull,
            Err(pull_requests::OperationError::Opened { number }) => {
                db::pull_requests::records::get_pull_request(pool, pinned.repository.id, number)
                    .await?
                    .ok_or(Refusal::Stale)?
            }
            Err(error) => return Err(error.into()),
        }
    };
    if pull.head_sha != update.commit_sha {
        records::manual(
            pool,
            job,
            key,
            &update.old_version,
            "An existing proposal was changed; review it manually rather than overwriting it",
        )
        .await?;
        return Ok(());
    }
    if pull.state == "open" {
        pull_requests::mutations::repair_open_pull(
            pool, ci, &pinned.repository, &pinned.repository, actor, &pull, true,
        )
        .await?;
    }
    records::attach_pull(
        pool,
        actor,
        pinned.repository.id,
        key,
        &update.commit_sha,
        pull.id,
    )
    .await?;
    Ok(())
}

pub(crate) async fn propose(
    pool: &db::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    client: &Client,
    job: &Job,
    pinned: &Pinned,
) -> Result<()> {
    let actor = job.updates_actor_id.ok_or(Refusal::Denied)?;
    let snapshot = records::snapshot(pool, actor, pinned.repository.id).await?;
    let mut proposals = snapshot.alerts.values().filter_map(|alert| alert.update.as_ref().map(|update| (
        (alert.finding.package.path.clone(), alert.finding.package.name.clone(), update.old_version.clone(), update.fixed_version.clone(), update.base_sha.clone()), update.clone(),
    ))).collect::<std::collections::HashMap<_, _>>();
    let packages = npm::Lockfile::parse(&pinned.manifest, &pinned.lock)?.packages;
    let store = repo_store(&pinned.repository, ci.cluster.as_ref());
    // The advisories without OSV records veto every candidate too, and they are read once for
    // the job. The OSV ones are looked up by package below.
    let advisories = db::dependabot_alerts::without_osv(pool).await?;
    let (mut attempted, mut proposed) = (0, 0);
    for (key, alert) in snapshot.alerts {
        if proposed >= 3 || attempted >= 5 {
            break;
        }
        if alert.state != AlertState::Open || alert.seen_sha != pinned.sha {
            continue;
        }
        if let Some(update) = &alert.update {
            if update.pull_request_id.is_some() {
                continue;
            }
            attempted += 1;
            let refs = store.refs_from_primary().await?;
            let reference = format!("refs/heads/{}", update.branch);
            let opened =
                records::proposal_number(pool, actor, pinned.repository.id, &update.branch)
                    .await?
                    .is_some();
            if opened || refs.get(&reference) == Some(&update.commit_sha) {
                // Automated proposals never execute repository-controlled workflows using
                // the administrator's delegation. A trusted user must explicitly run them.
                open(pool, ci, broadcaster, job, pinned, &key, update).await?;
                proposed += 1;
                continue;
            }
            if refs
                .get(&reference)
                .is_some_and(|sha| sha != &update.base_sha)
            {
                records::manual(
                    pool,
                    job,
                    &key,
                    &alert.finding.package.version,
                    "An automated branch changed; review it manually rather than overwriting it",
                )
                .await?;
                continue;
            }
        }
        let Some(package) = packages.iter().find(|p| {
            p.path == alert.finding.package.path
                && p.name == alert.finding.package.name
                && p.version.to_string() == alert.finding.package.version
        }) else {
            continue;
        };
        if package.path != format!("node_modules/{}", package.name)
            || package.version.major == 0
            || !package.version.pre.is_empty()
            || !package.version.build.is_empty()
            || alert.finding.fixed_versions.is_empty()
        {
            continue;
        }
        if attempted >= 5 {
            break;
        }
        attempted += 1;
        records::check_job(pool, job, Feature::Updates).await?;
        let old = match client
            .archive(&package.name, &package.version, Some(&package.integrity))
            .await
        {
            Ok(old) => old,
            // A network failure is retried by the worker. Only a refused archive is kept as a
            // manual task.
            Err(error) if error.is::<reqwest::Error>() => return Err(error),
            Err(_) => {
                records::manual(
                    pool,
                    job,
                    &key,
                    &alert.finding.package.version,
                    "The locked registry archive could not be verified; review or retry manually",
                )
                .await?;
                continue;
            }
        };
        let mut candidate = None;
        for version in alert.finding.fixed_versions.iter().take(3) {
            let version = semver::Version::parse(version)?;
            if version.major != package.version.major
                || version.minor != package.version.minor
                || version <= package.version
                || !version.pre.is_empty()
                || !version.build.is_empty()
            {
                continue;
            }
            let candidate_package = super::super::dependency_graph::Package {
                manifest: "package-lock.json".into(),
                ecosystem: "npm".into(),
                name: package.name.clone(),
                version: version.to_string(),
                package_url: format!("pkg:npm/{}@{}", package.name.replace('@', "%40"), version),
                scope: "unknown".into(),
            };
            if !crate::backend::vulnerabilities::findings(pool, &candidate_package)
                .await?
                .is_empty()
            {
                continue;
            }
            // Imported local advisories without OSV records must also veto a candidate.
            if advisories.iter().any(|a| {
                a["withdrawn_at"].is_null()
                    && a["vulnerabilities"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|v| {
                            v["package"]["ecosystem"] == "npm"
                                && v["package"]["name"] == package.name
                                && db::dependabot_alerts::affected(
                                    &version.to_string(),
                                    v["vulnerable_version_range"].as_str().unwrap_or(""),
                                )
                        })
            }) {
                continue;
            }
            records::check_job(pool, job, Feature::Updates).await?;
            let new = match client.archive(&package.name, &version, None).await {
                Ok(new) => new,
                // As with the locked archive, a network failure is retried, not made manual.
                Err(error) if error.is::<reqwest::Error>() => return Err(error),
                Err(_) => continue,
            };
            let patch = match npm::Lockfile::parse(&pinned.manifest, &pinned.lock)?.patch(
                &package.path,
                &old,
                &new,
            ) {
                Ok(patch) => patch,
                Err(_) => continue,
            };
            candidate = Some((version, patch));
            break;
        }
        let Some((version, patch)) = candidate else {
            records::manual(pool,job,&key,&alert.finding.package.version,"No non-vulnerable stable patch with verified unchanged requirements was found; update manually").await?;
            continue;
        };
        records::check_job(pool, job, Feature::Updates).await?;
        let target = (package.path.clone(), package.name.clone(), package.version.to_string(), version.to_string(), pinned.sha.clone());
        if let Some(update) = proposals.get(&target) {
            let mut receipt = update.clone();
            receipt.pull_request_id = None;
            records::plan_update(pool, job, &key, &receipt).await?;
            open(pool, ci, broadcaster, job, pinned, &key, &receipt).await?;
            continue;
        }
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&target)?));
        let branch = format!("codeyard/security/{digest}");
        if branch == pinned.repository.default_branch {
            records::manual(pool,job,&key,&alert.finding.package.version,"The generated proposal branch is the default branch; update manually or choose another default branch").await?;
            continue;
        }
        let reference = format!("refs/heads/{branch}");
        let refs = store.refs_from_primary().await?;
        if let Some(current) = refs.get(&reference) {
            if current != &pinned.sha {
                records::manual(
                    pool,
                    job,
                    &key,
                    &alert.finding.package.version,
                    "The proposed branch is already in use; no existing commits were overwritten",
                )
                .await?;
                continue;
            }
        } else {
            store
                .create_branch_as(pool, pinned.repository.id, actor, &branch, &pinned.sha)
                .await?;
        }
        let write = UploadWrite {
            branch: branch.clone(),
            message: format!("Update {} to {} for security review", package.name, version),
            expected_revision: pinned.sha.clone(),
            files: vec![
                UploadFile {
                    path: "package.json".into(),
                    size: patch.manifest.len(),
                },
                UploadFile {
                    path: "package-lock.json".into(),
                    size: patch.lock.len(),
                },
            ],
        };
        let mut bytes = patch.manifest;
        bytes.extend_from_slice(&patch.lock);
        let prepared = store
            .prepare_upload(
                &write,
                &bytes,
                "Codeyard security updates",
                &format!("{actor}@users.noreply.codeyard.invalid"),
            )
            .await?;
        let update = Update {
            branch,
            base_sha: pinned.sha.clone(),
            commit_sha: prepared.commit.sha.clone(),
            old_version: package.version.to_string(),
            fixed_version: version.to_string(),
            requested_by: actor,
            pull_request_id: None,
        };
        let publication = Publication {
            job: job.clone(),
            key: key.clone(),
            update: update.clone(),
        };
        if let Err(error) = store
            .publish_dependency_update_as(
                pool,
                pinned.repository.id,
                actor,
                &prepared,
                &publication,
            )
            .await
        {
            let refs = store.refs_from_primary().await?;
            if refs.get(&reference) != Some(&update.commit_sha) {
                if error.is_protected_branch() {
                    records::manual(pool,job,&key,&update.old_version,"Normal branch protection refused the update; administrator review is required").await?;
                    continue;
                }
                return Err(error.into());
            }
        }
        open(pool, ci, broadcaster, job, pinned, &key, &update).await?;
        proposals.insert(target, update);
        proposed += 1;
    }
    Ok(())
}
