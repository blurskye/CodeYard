//! Leased proposals consume the inventory and alerts maintained by the vulnerability worker.
use anyhow::Context;

use crate::repositories::dependencies::registry::Client;
use anyhow::ensure;
use anyhow::Result;
use codeyard_ci::runs::CiRuns;
use codeyard_database::models::Repository;
use codeyard_db_repos::repositories::dependencies::Feature;
use codeyard_db_repos::repositories::dependencies::Job;
use codeyard_db_repos::repositories::dependencies::Refusal;
use codeyard_git::error::ErrorKind;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_git_storage::repositories::store::RepoStore;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_storage::range::ByteRange as BlobRange;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Unsupported(&'static str);
pub(crate) struct Pinned {
    pub repository: Repository,
    pub sha: String,
    pub manifest: Vec<u8>,
    pub lock: Vec<u8>,
}
async fn file(store: &RepoStore, sha: &str, path: &str, limit: usize) -> Result<Vec<u8>> {
    let blob = store
        .blob_range(
            sha,
            path,
            BlobRange::Window {
                offset: 0,
                length: (limit + 1) as u64,
            },
        )
        .await
        .map_err(|e| {
            if e.kind == ErrorKind::MissingPath {
                anyhow::Error::new(Unsupported(
                    "Only root package.json and package-lock.json are supported",
                ))
            } else {
                e.into()
            }
        })?;
    ensure!(
        matches!(blob.mode, 0o100644 | 0o100755) && !blob.is_binary && blob.size <= limit as u64,
        Unsupported("Dependency files must be bounded regular UTF-8 JSON files")
    );
    Ok(blob.bytes)
}
async fn process(
    pool: &codeyard_database::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    client: &Client,
    job: &Job,
) -> Result<()> {
    codeyard_db_repos::repositories::dependencies::check_job(pool, job, Feature::Alerts).await?;
    let repository =
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, job.repo_id)
            .await?
            .ok_or(Refusal::Missing)?;
    let store = repo_store(&repository, ci.cluster.as_ref());
    let refs = store.refs_from_primary().await?;
    if refs.is_empty() {
        return Err(Unsupported("No default commit exists yet").into());
    }
    let stored = store.info().await?.default_branch;
    let branch = codeyard_git_storage::repositories::reads::default_branch(
        &refs,
        &stored,
        &repository.default_branch,
    );
    let sha = refs
        .get(&format!("refs/heads/{branch}"))
        .context("The default branch is missing")?
        .clone();
    // A proposal must use the shared scanner's current pinned inventory, never its own scan.
    codeyard_db_repos::repositories::dependencies::complete_scan(pool, job, &sha).await?;
    if job.updates_actor_id.is_some() {
        // Only proposals need an npm lockfile. Without a usable one they are skipped, and the
        // alerts from the shared scan stay active. A storage failure is still retried.
        let (manifest, lock) = match (
            file(
                &store,
                &sha,
                "package.json",
                codeyard_repos::dependencies::npm::MAX_MANIFEST,
            )
            .await,
            file(
                &store,
                &sha,
                "package-lock.json",
                codeyard_repos::dependencies::npm::MAX_LOCK_BYTES,
            )
            .await,
        ) {
            (Ok(manifest), Ok(lock))
                if codeyard_repos::dependencies::npm::Lockfile::parse(&manifest, &lock)
                    .is_ok_and(|parsed| parsed.unsupported == 0) =>
            {
                (manifest, lock)
            }
            (Err(error), _) | (_, Err(error)) if !error.is::<Unsupported>() => return Err(error),
            _ => {
                tracing::info!(
                    repo_id = repository.id,
                    "Skipped security updates without a usable npm lockfile"
                );
                return Ok(());
            }
        };
        match codeyard_db_repos::repositories::dependencies::check_job(pool, job, Feature::Updates)
            .await
        {
            Ok(()) => {
                crate::repositories::dependencies::updates::propose(
                    pool,
                    ci,
                    broadcaster,
                    client,
                    job,
                    &Pinned {
                        repository,
                        sha,
                        manifest,
                        lock,
                    },
                )
                .await?
            }
            Err(error) if matches!(error.downcast_ref::<Refusal>(), Some(Refusal::Denied)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
/// The production worker entry, also usable with trusted loopback providers in integration tests.
pub async fn run_once(
    pool: &codeyard_database::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    client: &Client,
) -> Result<bool> {
    let Some(job) = codeyard_db_repos::repositories::dependencies::claim(
        pool,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await?
    else {
        return Ok(false);
    };
    let result = tokio::time::timeout(
        Duration::from_secs(150),
        process(pool, ci, broadcaster, client, &job),
    )
    .await;
    let (status, message) = match &result {
        Ok(Ok(())) => (None, None),
        Err(_) => (
            Some("error"),
            Some("Dependency security work exceeded its bounded deadline"),
        ),
        Ok(Err(error)) => {
            if let Some(unsupported) = error.downcast_ref::<Unsupported>() {
                (Some("unsupported"), Some(unsupported.0))
            } else if matches!(error.downcast_ref::<Refusal>(), Some(Refusal::Denied)) {
                (
                    Some("paused"),
                    Some("The enabling administrator is no longer authorized"),
                )
            } else {
                (Some("error"),Some("Dependency security work failed; verify root npm files, provider connectivity and repository storage"))
            }
        }
    };
    if status.is_some() {
        tracing::warn!(
            repo_id = job.repo_id,
            "Dependency security job did not complete; its private status was retained"
        );
    }
    codeyard_db_repos::repositories::dependencies::release(pool, &job, status, message).await?;
    Ok(true)
}
/// Pinned to the web-server lifetime; no detached scanner or unbounded per-repository task set.
pub async fn run(
    pool: codeyard_database::DbPool,
    ci: std::sync::Arc<CiRuns>,
    broadcaster: std::sync::Arc<NotificationBroadcaster>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            return;
        }
        let client = match Client::new("https://registry.npmjs.org/") {
            Ok(client) => client,
            Err(_) => {
                tracing::warn!("Dependency provider client unavailable");
                tokio::select! {()=stopped(&mut stop)=>return,()=tokio::time::sleep(Duration::from_secs(60))=>{}}
                continue;
            }
        };
        loop {
            if *stop.borrow() {
                return;
            }
            tokio::select! {
                ()=stopped(&mut stop)=>return,
                result=run_once(&pool,&ci,&broadcaster,&client)=>match result{
                    Ok(true)=>{},Ok(false)=>tokio::time::sleep(Duration::from_secs(5)).await,
                    Err(_)=>{tracing::warn!("Dependency job queue unavailable");tokio::time::sleep(Duration::from_secs(30)).await;}
                }
            }
        }
    }
}
async fn stopped(stop: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}
