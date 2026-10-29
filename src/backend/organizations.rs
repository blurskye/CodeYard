//! Shared organization lifecycle effects for native settings and REST.
use crate::backend::{
    app::AppState,
    db::{organizations, DbPool},
    repositories::lifecycle,
};
use organizations::OrganizationOutcome::{self, Applied};
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

static CONVERSIONS: LazyLock<TaskTracker> = LazyLock::new(TaskTracker::new);
static CONVERSION_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(16)));

/// Admission is bounded and checks the same owner/last-owner rules as the eventual transaction.
/// The worker rechecks all authority rather than turning the preflight read into a write permit.
pub async fn enqueue_outside_conversion(
    pool: &DbPool,
    actor: i64,
    org: i64,
    target: i64,
) -> anyhow::Result<OrganizationOutcome<()>> {
    match organizations::prepare_outside_conversion(pool, actor, org, target).await? {
        Applied(()) => {}
        other => return Ok(other),
    }
    anyhow::ensure!(
        !CONVERSIONS.is_closed(),
        "Organization workers are stopping"
    );
    let permit = CONVERSION_SLOTS
        .clone()
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("Organization conversion queue is full"))?;
    let pool = pool.clone();
    CONVERSIONS.spawn(async move {
        let _permit = permit;
        match organizations::convert_to_outside_collaborator(&pool, actor, org, target).await {
            Ok(Applied(())) => {}
            outcome => tracing::warn!(
                actor,
                org,
                target,
                ?outcome,
                "queued organization conversion was refused"
            ),
        }
    });
    Ok(Applied(()))
}

pub(crate) async fn shutdown(deadline: tokio::time::Instant) {
    CONVERSIONS.close();
    if tokio::time::timeout_at(deadline, CONVERSIONS.wait())
        .await
        .is_err()
    {
        tracing::warn!("organization conversion shutdown grace expired");
    }
}

/// Deletes repositories first, then the organization. A partial failure leaves the remaining
/// repositories and organization available for a retry; concurrent creation refuses the final delete.
pub async fn delete(
    state: &AppState,
    actor: i64,
    org: i64,
) -> anyhow::Result<OrganizationOutcome<()>> {
    let repositories =
        match organizations::list_organization_deletion_repositories(&state.pool, actor, org)
            .await?
        {
            Applied(repositories) => repositories,
            other => return Ok(other.map(|_| ())),
        };
    for repository in repositories {
        let deleted = match organizations::delete_organization_repository(
            &state.pool,
            actor,
            org,
            repository,
        )
        .await?
        {
            Applied(deleted) => deleted,
            other => return Ok(other.map(|_| ())),
        };
        lifecycle::remove_deleted_repository(state.cluster.as_ref(), &deleted).await?;
    }
    organizations::delete_organization(&state.pool, actor, org).await
}
