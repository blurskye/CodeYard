//! Traffic means observed native navigations and completed initial Git object transfers.
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_db_core::DbPool;
use codeyard_db_repos::repositories::traffic::Identity;
use codeyard_db_repos::repositories::traffic::Kind;
use codeyard_db_repos::repositories::traffic::Metric;
use codeyard_db_repos::repositories::traffic::PopularPath;
use codeyard_db_repos::repositories::traffic::PopularReferrer;
use codeyard_db_repos::repositories::traffic::Summary;
use codeyard_db_repos::repositories::traffic::Window;
pub mod clone;

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Repository not found")]
    Missing,
    #[error("Repository push access is required to view traffic")]
    Denied,
    #[error("per must be day or week")]
    Period,
}
pub fn weekly(per: Option<&str>) -> Result<bool> {
    match per {
        None | Some("day") => Ok(false),
        Some("week") => Ok(true),
        _ => Err(Refusal::Period.into()),
    }
}
async fn authorize(pool: &DbPool, viewer: Option<&User>, repo: &Repository) -> Result<()> {
    ensure!(
        codeyard_identity::repository_access::can_read(pool, viewer, repo).await?,
        Refusal::Missing
    );
    let viewer = viewer.ok_or(Refusal::Denied)?;
    ensure!(
        codeyard_identity::repository_access::user_access(pool, viewer, repo)
            .await?
            .can_write(),
        Refusal::Denied
    );
    Ok(())
}
pub async fn summary(
    pool: &DbPool,
    viewer: Option<&User>,
    repo: &Repository,
    kind: Kind,
    weekly: bool,
) -> Result<Summary> {
    authorize(pool, viewer, repo).await?;
    codeyard_db_repos::repositories::traffic::summary(
        pool,
        repo.id,
        kind,
        &Window::current(),
        weekly,
    )
    .await
}
pub async fn paths(
    pool: &DbPool,
    viewer: Option<&User>,
    repo: &Repository,
) -> Result<Vec<PopularPath>> {
    authorize(pool, viewer, repo).await?;
    codeyard_db_repos::repositories::traffic::paths(pool, repo.id, &Window::current()).await
}
pub async fn referrers(
    pool: &DbPool,
    viewer: Option<&User>,
    repo: &Repository,
) -> Result<Vec<PopularReferrer>> {
    authorize(pool, viewer, repo).await?;
    codeyard_db_repos::repositories::traffic::referrers(pool, repo.id, &Window::current()).await
}
/// One bounded cleanup batch, shared by the live service and maintenance tests.
pub async fn purge_expired(pool: &DbPool) -> Result<u64> {
    codeyard_db_repos::repositories::traffic::prune(pool, &Window::current().first).await
}
pub async fn maintain(pool: DbPool, mut stop: tokio::sync::watch::Receiver<bool>) {
    use tokio::time::{Duration, Instant, MissedTickBehavior};
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {biased;_ = async {let _=stop.wait_for(|stopped|*stopped).await;}=>break,_ = interval.tick()=>{}}
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout(Duration::from_secs(2), purge_expired(&pool)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {}
                _ => {
                    tracing::warn!("Could not expire repository traffic observations");
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

/// How long one observation may take to save. It is saved before the response ends so the
/// counts are exact; a busy machine can stall a write for seconds, a dead database cannot
/// hold a page longer than this.
const OBSERVE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// The read/transfer has already happened. An analytics outage must not turn it into a
/// fabricated Git failure; persist under a short budget and report bookkeeping failures.
pub async fn observe(pool: &DbPool, repo: i64, identity: &Identity, metric: &Metric) {
    match tokio::time::timeout(
        OBSERVE_BUDGET,
        codeyard_db_repos::repositories::traffic::record(pool, repo, identity, metric),
    )
    .await
    {
        Ok(Ok(())) => {}
        _ => tracing::warn!(
            repo_id = repo,
            "Could not persist repository traffic observation"
        ),
    }
}
